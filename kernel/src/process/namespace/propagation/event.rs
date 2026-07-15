//! Mount, move, and unmount event propagation across peer/slave topology.

use alloc::collections::{BTreeMap, VecDeque};
use alloc::sync::Arc;
use alloc::vec::Vec;

use hashbrown::{HashMap, HashSet};
use system_error::SystemError;

use crate::filesystem::vfs::{
    mount::{MountEdgeReservation, MountFSInode},
    MountFS,
};

use super::change::PreparedPropagationRemoval;
use super::group::{
    apply_prepared_peer_groups, get_peers, prepare_peer_registrations, PreparedPeerGroupState,
    PropagationGroup,
};
use super::state::{register_slave_with_master, PropagationType};

#[derive(Clone, Copy)]
enum PropagationTargetKind {
    Peer,
    Slave,
}

struct PropagationTarget {
    mount: Arc<MountFS>,
    kind: PropagationTargetKind,
}

struct PreparedMount {
    target_parent: Arc<MountFS>,
    mountpoint: Arc<MountFSInode>,
    expected_top: Option<Arc<MountFS>>,
    clone: Arc<MountFS>,
    _target_reservation: Option<MountEdgeReservation>,
    cover_reservation: Option<MountEdgeReservation>,
}

pub(crate) struct PreparedPropagation {
    source_mnt: Arc<MountFS>,
    mountpoint: Arc<MountFSInode>,
    new_child: Arc<MountFS>,
    mounts: Vec<PreparedMount>,
    registrations: PreparedRegistrations,
    _local_reservation: MountEdgeReservation,
}

pub(super) struct CorrespondingSources {
    mounts: BTreeMap<usize, Arc<MountFS>>,
    peer_groups: BTreeMap<usize, Arc<MountFS>>,
}

impl CorrespondingSources {
    pub(super) fn new() -> Self {
        Self {
            mounts: BTreeMap::new(),
            peer_groups: BTreeMap::new(),
        }
    }

    pub(super) fn insert(&mut self, parent: &Arc<MountFS>, child: Arc<MountFS>) {
        self.mounts.insert(parent.mount_id().data(), child.clone());
        let propagation = parent.propagation();
        let group_id = propagation.peer_group_id();
        if propagation.is_shared() && group_id.is_valid() {
            // Linux propagate_one() advances last_source after every
            // successfully materialized peer. Keep the newest child for the
            // group so a later covered slave of an uncovered peer inherits
            // from that nearest source rather than the event root.
            self.peer_groups.insert(group_id.data(), child);
        }
    }

    pub(super) fn nearest(
        &self,
        target: &Arc<MountFS>,
    ) -> Result<Option<Arc<MountFS>>, SystemError> {
        // Linux `propagate_one()` retains `last_source` when a narrow peer is
        // uncovered. Match that layer before walking to the next master.
        let mut visited = HashSet::new();
        visited.insert(target.mount_id().data());
        let mut master = target.propagation().master();
        while let Some(candidate) = master {
            if !visited.insert(candidate.mount_id().data()) {
                return Err(SystemError::ELOOP);
            }
            if let Some(source) = self.mounts.get(&candidate.mount_id().data()) {
                return Ok(Some(source.clone()));
            }
            let propagation = candidate.propagation();
            let group_id = propagation.peer_group_id();
            if propagation.is_shared() && group_id.is_valid() {
                if let Some(source) = self.peer_groups.get(&group_id.data()) {
                    return Ok(Some(source.clone()));
                }
            }
            master = propagation.master();
        }
        Ok(None)
    }
}

struct PreparedRegistrations {
    peer_groups: Vec<PreparedPeerGroupState>,
    slaves: Vec<Arc<MountFS>>,
}

impl PreparedRegistrations {
    fn prepare(mounts: &[Arc<MountFS>]) -> Result<Self, SystemError> {
        let peer_groups = prepare_peer_registrations(mounts)?;
        let mut slaves = Vec::new();
        slaves
            .try_reserve(mounts.len())
            .map_err(|_| SystemError::ENOMEM)?;
        for mount in mounts {
            // Live source mounts in a move already occupy their master's
            // reverse list. Detached source/clone mounts need publication.
            if !mount.is_live() && mount.propagation().master().is_some() {
                slaves.push(mount.clone());
            }
        }

        let mut masters = Vec::new();
        masters
            .try_reserve(slaves.len())
            .map_err(|_| SystemError::ENOMEM)?;
        for slave in &slaves {
            let master = slave
                .propagation()
                .master()
                .expect("prepared slave retained its master");
            masters.push((master.mount_id().data(), master));
        }
        masters.sort_unstable_by_key(|(mount_id, _)| *mount_id);
        let mut index = 0;
        while index < masters.len() {
            let start = index;
            let master_id = masters[index].0;
            while index < masters.len() && masters[index].0 == master_id {
                index += 1;
            }
            masters[start]
                .1
                .propagation()
                .try_reserve_slaves(index - start)?;
        }
        Ok(Self {
            peer_groups,
            slaves,
        })
    }

    fn commit(self) {
        apply_prepared_peer_groups(self.peer_groups);
        for mount in self.slaves {
            register_slave_with_master(&mount);
        }
    }
}

/// Return every peer/slave destination exactly once.  The registry is only a
/// discovery index; the mount/dentry objects below remain the correctness
/// identity throughout prepare and commit.
fn propagation_targets(source: &Arc<MountFS>) -> Vec<PropagationTarget> {
    let source_group = source.propagation().peer_group_id();
    let peers = get_peers(source_group, source);
    let mut result = Vec::new();
    let mut visited = HashSet::new();
    visited.insert(source.mount_id().data());

    for peer in peers {
        if visited.insert(peer.mount_id().data()) {
            result.push(PropagationTarget {
                mount: peer.clone(),
                kind: PropagationTargetKind::Peer,
            });
        }
    }

    let mut pending = source.propagation().slaves();
    for peer in result.iter().map(|target| &target.mount) {
        pending.extend(peer.propagation().slaves());
    }
    while let Some(slave) = pending.pop() {
        if !visited.insert(slave.mount_id().data()) {
            continue;
        }
        pending.extend(slave.propagation().slaves());
        result.push(PropagationTarget {
            mount: slave,
            kind: PropagationTargetKind::Slave,
        });
    }
    result
}

/// Apply Linux's peer/slave clone flags to one detached copy.
fn configure_clone_propagation(
    source: &Arc<MountFS>,
    clone: &Arc<MountFS>,
    target_parent: &Arc<MountFS>,
    kind: PropagationTargetKind,
    master_source: Option<&Arc<MountFS>>,
    slave_groups: &mut BTreeMap<(usize, usize), Arc<PropagationGroup>>,
) -> Result<(), SystemError> {
    if matches!(kind, PropagationTargetKind::Peer) {
        return Ok(());
    }

    let clone_prop = clone.propagation();
    clone_prop.set_private();
    let master_source = master_source.expect("slave propagation requires the previous layer");
    clone_prop.set_slave(Some(Arc::downgrade(master_source)));

    // A shared slave parent needs one corresponding child peer group across
    // all of its peers.  Keying by object identities avoids pathname aliases.
    let target_prop = target_parent.propagation();
    if target_prop.is_shared() {
        let key = (target_prop.peer_group_id().data(), source.mount_id().data());
        let group = match slave_groups.entry(key) {
            alloc::collections::btree_map::Entry::Occupied(entry) => entry.get().clone(),
            alloc::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(PropagationGroup::alloc()?).clone()
            }
        };
        clone_prop.set_shared_with_group(group);
    }
    Ok(())
}

/// Build a complete detached copy.  No namespace, lifecycle, peer registry or
/// master/slave list is published during this phase.
fn prepare_subtree_copy(
    source: &Arc<MountFS>,
    target_parent: &Arc<MountFS>,
    mountpoint: Arc<MountFSInode>,
    kind: PropagationTargetKind,
    master_root: Option<&Arc<MountFS>>,
    slave_groups: &mut BTreeMap<(usize, usize), Arc<PropagationGroup>>,
) -> Result<Arc<MountFS>, SystemError> {
    let root_clone = source.deepcopy(Some(mountpoint))?;
    configure_clone_propagation(
        source,
        &root_clone,
        target_parent,
        kind,
        master_root,
        slave_groups,
    )?;

    let build_result = (|| {
        // An explicit queue prevents adversarially deep mount trees from
        // exhausting the kernel stack. Replaying each stack bottom-to-top
        // preserves shadow order in the detached clone.
        let mut queue = vec![(source.clone(), root_clone.clone(), master_root.cloned())];
        let mut index = 0;
        while index < queue.len() {
            let (source_parent, clone_parent, master_parent) = queue[index].clone();
            index += 1;
            let child_stacks: Vec<Vec<Arc<MountFS>>> =
                source_parent.mountpoints().values().cloned().collect();
            for stack in child_stacks {
                for (stack_index, source_child) in stack.into_iter().enumerate() {
                    let source_mp = source_child.self_mountpoint().ok_or(SystemError::EINVAL)?;
                    let clone_mp = clone_parent.wrapper_for_dentry(source_mp.shared_dentry())?;
                    let child_clone = source_child.deepcopy(Some(clone_mp.clone()))?;
                    let master_child = master_parent.as_ref().and_then(|master_parent| {
                        let master_mp =
                            master_parent.wrapper_for_existing_edge(source_mp.shared_dentry());
                        master_parent
                            .children_at(&master_mp)
                            .get(stack_index)
                            .cloned()
                    });
                    if matches!(kind, PropagationTargetKind::Slave) && master_child.is_none() {
                        return Err(SystemError::EBUSY);
                    }
                    configure_clone_propagation(
                        &source_child,
                        &child_clone,
                        &clone_parent,
                        kind,
                        master_child.as_ref(),
                        slave_groups,
                    )?;
                    if let Err(error) = clone_parent.attach_top(&clone_mp, child_clone.clone()) {
                        MountFS::deactivate_disconnected_subtree(&child_clone);
                        return Err(error);
                    }
                    queue.push((source_child, child_clone, master_child));
                }
            }
        }
        Ok(())
    })();
    if let Err(error) = build_result {
        // Detached mount trees contain strong parent/child cycles. Break every
        // edge on prepare failure even though no lifecycle counters were
        // published yet.
        MountFS::deactivate_disconnected_subtree(&root_clone);
        return Err(error);
    }
    Ok(root_clone)
}

fn abandon_prepared(prepared: &[PreparedMount]) {
    for item in prepared {
        MountFS::deactivate_disconnected_subtree(&item.clone);
    }
}

pub(crate) fn abort_mount_propagation(prepared: Option<PreparedPropagation>) {
    if let Some(prepared) = prepared {
        abandon_prepared(&prepared.mounts);
    }
}

fn collect_subtree(root: &Arc<MountFS>) -> Vec<Arc<MountFS>> {
    let mut result = Vec::new();
    let mut pending = vec![root.clone()];
    while let Some(mount) = pending.pop() {
        pending.extend(mount.mount_children());
        result.push(mount);
    }
    result
}

/// Reserve every peer-group ID before changing a detached subtree. This is the
/// object-topology equivalent of Linux `invent_group_ids(..., true)` and keeps
/// recursive publication all-or-nothing on allocation failure.
pub(crate) fn ensure_subtree_shared(root: &Arc<MountFS>) -> Result<(), SystemError> {
    let subtree = collect_subtree(root);
    let mut groups = Vec::new();
    for mount in &subtree {
        if !mount.propagation().is_shared() {
            groups.push((mount.clone(), PropagationGroup::alloc()?));
        }
    }
    for (mount, group) in groups {
        mount.propagation().set_shared_with_group(group);
    }
    Ok(())
}

fn activate_subtree(
    root: &Arc<MountFS>,
    namespace: Option<&Arc<super::super::mnt::MntNamespace>>,
) -> Result<(), SystemError> {
    for mount in collect_subtree(root) {
        if let Some(namespace) = namespace {
            mount.set_namespace(Arc::downgrade(namespace));
        }
        mount.activate()?;
    }
    Ok(())
}

pub(crate) fn prepare_mount_propagation_locked(
    source_mnt: &Arc<MountFS>,
    mountpoint: &Arc<MountFSInode>,
    new_child: &Arc<MountFS>,
) -> Result<Option<PreparedPropagation>, SystemError> {
    let source_prop = source_mnt.propagation();
    let canonical_mountpoint = source_mnt.wrapper_for_dentry(mountpoint.shared_dentry())?;
    if canonical_mountpoint.dentry_id() != mountpoint.dentry_id() {
        return Err(SystemError::EINVAL);
    }
    // New/bind and move both publish the local source as a new top edge.
    // Reserve its parent key/stack before either path changes topology.
    let local_reservation = source_mnt.reserve_mount_edge(&canonical_mountpoint, 1)?;

    let source_dentry = mountpoint.shared_dentry();
    let mut slave_groups = BTreeMap::new();
    let mut mounts = Vec::new();
    let mut propagated_sources = CorrespondingSources::new();
    propagated_sources.insert(source_mnt, new_child.clone());
    let targets = if source_prop.is_shared() {
        propagation_targets(source_mnt)
    } else {
        Vec::new()
    };
    for target in targets {
        let PropagationTarget {
            mount: target_parent,
            kind,
        } = target;
        let master_source = if matches!(kind, PropagationTargetKind::Slave) {
            match propagated_sources.nearest(&target_parent) {
                Ok(source) => source,
                Err(error) => {
                    abandon_prepared(&mounts);
                    return Err(error);
                }
            }
        } else {
            None
        };
        if matches!(kind, PropagationTargetKind::Slave) && master_source.is_none() {
            continue;
        }
        let target_mp = match target_parent.wrapper_for_dentry(source_dentry.clone()) {
            Ok(mountpoint) => mountpoint,
            // Equivalent to Linux propagate_mnt skipping a peer whose bind
            // root does not cover the source mountpoint.
            Err(SystemError::EXDEV) => continue,
            Err(error) => {
                abandon_prepared(&mounts);
                return Err(error);
            }
        };
        let expected_top = target_parent.lookup_top(&target_mp);
        let clone = match prepare_subtree_copy(
            new_child,
            &target_parent,
            target_mp.clone(),
            kind,
            master_source.as_ref(),
            &mut slave_groups,
        ) {
            Ok(clone) => clone,
            Err(error) => {
                abandon_prepared(&mounts);
                return Err(error);
            }
        };
        let target_reservation = if expected_top.is_none() {
            match target_parent.reserve_mount_edge(&target_mp, 1) {
                Ok(reservation) => Some(reservation),
                Err(error) => {
                    MountFS::deactivate_disconnected_subtree(&clone);
                    abandon_prepared(&mounts);
                    return Err(error);
                }
            }
        } else {
            None
        };
        let cover_reservation = if expected_top.is_some() {
            let cover_mountpoint = clone.mountpoint_root_inode();
            match clone.reserve_mount_edge(&cover_mountpoint, 1) {
                Ok(reservation) => Some(reservation),
                Err(error) => {
                    MountFS::deactivate_disconnected_subtree(&clone);
                    abandon_prepared(&mounts);
                    return Err(error);
                }
            }
        } else {
            None
        };
        propagated_sources.insert(&target_parent, clone.clone());
        mounts.push(PreparedMount {
            target_parent,
            mountpoint: target_mp,
            expected_top,
            clone,
            _target_reservation: target_reservation,
            cover_reservation,
        });
    }
    let mut registration_mounts = collect_subtree(new_child);
    for item in &mounts {
        registration_mounts.extend(collect_subtree(&item.clone));
    }
    let registrations = match PreparedRegistrations::prepare(&registration_mounts) {
        Ok(registrations) => registrations,
        Err(error) => {
            abandon_prepared(&mounts);
            return Err(error);
        }
    };
    Ok(Some(PreparedPropagation {
        source_mnt: source_mnt.clone(),
        mountpoint: canonical_mountpoint,
        new_child: new_child.clone(),
        mounts,
        registrations,
        _local_reservation: local_reservation,
    }))
}

pub(crate) fn commit_mount_propagation_locked(
    prepared: Option<PreparedPropagation>,
) -> Result<(), SystemError> {
    let Some(prepared) = prepared else {
        return Ok(());
    };
    if !prepared.source_mnt.is_live()
        || !prepared
            .source_mnt
            .children_at(&prepared.mountpoint)
            .iter()
            .any(|child| Arc::ptr_eq(child, &prepared.new_child))
    {
        abandon_prepared(&prepared.mounts);
        return Err(SystemError::EBUSY);
    }
    for item in &prepared.mounts {
        if !item.target_parent.is_live() {
            abandon_prepared(&prepared.mounts);
            return Err(SystemError::EBUSY);
        }
        // A rename may have moved the shared dentry outside a bind root after
        // prepare. Re-project it while serialized instead of trusting the old
        // wrapper's pathname ancestry.
        let current_mountpoint = match item
            .target_parent
            .wrapper_for_dentry(item.mountpoint.shared_dentry())
        {
            Ok(mountpoint) => mountpoint,
            Err(_) => {
                abandon_prepared(&prepared.mounts);
                return Err(SystemError::EBUSY);
            }
        };
        if current_mountpoint.dentry_id() != item.mountpoint.dentry_id() {
            abandon_prepared(&prepared.mounts);
            return Err(SystemError::EBUSY);
        }
        let current = item.target_parent.lookup_top(&item.mountpoint);
        let unchanged = match (&current, &item.expected_top) {
            (None, None) => true,
            (Some(current), Some(expected)) => Arc::ptr_eq(current, expected),
            _ => false,
        };
        if !unchanged {
            abandon_prepared(&prepared.mounts);
            return Err(SystemError::EBUSY);
        }
    }

    // Namespace/lifecycle are initialized before any edge becomes reachable.
    let source_namespace = prepared.new_child.namespace();
    for item in &prepared.mounts {
        let namespace = item.target_parent.namespace();
        if let (Some(source_namespace), Some(target_namespace)) =
            (source_namespace.as_ref(), namespace.as_ref())
        {
            if !Arc::ptr_eq(source_namespace.user_ns(), target_namespace.user_ns()) {
                // Linux lock_mnt_tree() protects mounts propagated across a
                // user-namespace boundary, while leaving the propagated root
                // itself movable as the new visible boundary.
                for mount in collect_subtree(&item.clone) {
                    mount.lock_mount();
                }
                item.clone.unlock_mount();
            }
        }
        if let Err(error) = activate_subtree(&item.clone, namespace.as_ref()) {
            abandon_prepared(&prepared.mounts);
            return Err(error);
        }
    }

    let mut attached: Vec<&PreparedMount> = Vec::new();
    if attached.try_reserve(prepared.mounts.len()).is_err() {
        abandon_prepared(&prepared.mounts);
        return Err(SystemError::ENOMEM);
    }
    for item in &prepared.mounts {
        let result = if let Some(cover_reservation) = item.cover_reservation.as_ref() {
            item.target_parent.attach_beneath_prepared(
                &item.mountpoint,
                item.clone.clone(),
                cover_reservation.mountpoint(),
            )
        } else {
            item.target_parent
                .attach_top(&item.mountpoint, item.clone.clone())
        };
        if let Err(error) = result {
            for committed in attached.iter().rev() {
                committed
                    .target_parent
                    .detach_exact_restoring_prepared_cover(
                        &committed.clone,
                        committed
                            .cover_reservation
                            .as_ref()
                            .map(MountEdgeReservation::mountpoint),
                        committed.expected_top.as_ref(),
                    )
                    .expect("propagation rollback must restore every exact mount edge");
            }
            abandon_prepared(&prepared.mounts);
            return Err(error);
        }
        attached.push(item);
    }

    prepared.registrations.commit();
    Ok(())
}

/// Linux makes every mount in a moved tree shared before propagating the tree
/// into the destination parent's peers.  The complete tree is copied once per
/// destination instead of the former path/BFS reconstruction.
pub(crate) struct PreparedMovePropagation {
    propagation: Option<PreparedPropagation>,
    invented: Vec<(Arc<MountFS>, PropagationType, Arc<PropagationGroup>)>,
}

fn restore_invented_groups(invented: Vec<(Arc<MountFS>, PropagationType, Arc<PropagationGroup>)>) {
    for (mount, old_type, _) in invented.into_iter().rev() {
        let prop = mount.propagation();
        match old_type {
            PropagationType::Private => prop.set_private(),
            PropagationType::Slave => {
                prop.clear_shared();
                prop.clear_group_id();
            }
            PropagationType::Unbindable => prop.set_unbindable(),
            PropagationType::Shared => unreachable!(),
        }
    }
}

pub(crate) fn prepare_moved_tree_propagation_locked(
    target_parent: &Arc<MountFS>,
    moved_root: &Arc<MountFS>,
    moved_root_mountpoint: &Arc<MountFSInode>,
) -> Result<PreparedMovePropagation, SystemError> {
    let subtree = collect_subtree(moved_root);
    let mut invented = Vec::new();
    for mount in &subtree {
        let prop = mount.propagation();
        if !prop.is_shared() {
            invented.push((mount.clone(), prop.prop_type(), PropagationGroup::alloc()?));
        }
    }
    for (mount, _, group) in &invented {
        let prop = mount.propagation();
        if !prop.is_shared() {
            prop.set_shared_with_group(group.clone());
        }
    }
    let propagation =
        match prepare_mount_propagation_locked(target_parent, moved_root_mountpoint, moved_root) {
            Ok(propagation) => propagation,
            Err(error) => {
                restore_invented_groups(invented);
                return Err(error);
            }
        };
    Ok(PreparedMovePropagation {
        propagation,
        invented,
    })
}

pub(crate) fn abort_moved_tree_propagation_locked(prepared: PreparedMovePropagation) {
    abort_mount_propagation(prepared.propagation);
    restore_invented_groups(prepared.invented);
}

pub(crate) fn commit_moved_tree_propagation_locked(
    prepared: PreparedMovePropagation,
) -> Result<(), SystemError> {
    if let Err(error) = commit_mount_propagation_locked(prepared.propagation) {
        restore_invented_groups(prepared.invented);
        return Err(error);
    }
    // The propagation registration plan includes invented source groups and
    // publishes them together with clone membership after every edge commits.
    drop(prepared.invented);
    Ok(())
}

/// Propagate an umount event to all peers and slaves.
///
/// When a mount is unmounted from a shared mount point, this function
/// propagates the umount to all peers in the same group and all slaves.
///
/// # Arguments
/// * `parent_mnt` - The parent mount where the umount occurred
/// * `mountpoint` - The exact shared dentry where the event occurred
///
/// # Returns
/// * `Ok(())` on success
/// * `Err(SystemError)` on failure
#[cfg(test)]
pub(super) fn propagate_umount(
    parent_mnt: &Arc<MountFS>,
    mountpoint: &Arc<MountFSInode>,
    source_child: &Arc<MountFS>,
    lazy: bool,
) -> Result<(), SystemError> {
    if source_child
        .self_mountpoint()
        .as_ref()
        .is_none_or(|source_mp| !Arc::ptr_eq(source_mp, mountpoint))
        || !Arc::ptr_eq(&mountpoint.mount_fs(), parent_mnt)
    {
        return Err(SystemError::EINVAL);
    }
    propagate_umount_sources(core::slice::from_ref(source_child), lazy)
}

/// Propagate removal of a complete local umount list. `sources` is ordered
/// deepest-first, matching `MountFS::umount_subtree_with_mode()`.
pub(crate) fn propagate_umount_sources(
    sources: &[Arc<MountFS>],
    lazy: bool,
) -> Result<(), SystemError> {
    for source in sources {
        let mountpoint = source.self_mountpoint().ok_or(SystemError::EINVAL)?;
        if !mountpoint
            .mount_fs()
            .children_at(&mountpoint)
            .iter()
            .any(|child| Arc::ptr_eq(child, source))
        {
            return Err(SystemError::EBUSY);
        }
    }
    let mut prepared = prepare_propagated_umount_targets(sources, lazy, true)?;
    let mut graph_targets = Vec::new();
    let graph_target_count = sources
        .len()
        .checked_add(prepared.len())
        .ok_or(SystemError::ENOMEM)?;
    graph_targets
        .try_reserve(graph_target_count)
        .map_err(|_| SystemError::ENOMEM)?;
    graph_targets.extend(sources.iter().cloned());
    graph_targets.extend(
        prepared
            .iter()
            .filter(|target| target.remove)
            .map(|target| target.child.clone()),
    );
    let graph = PreparedPropagationRemoval::prepare_locked(&graph_targets)?;

    // The caller serializes detach through MOUNT_LIFECYCLE_LOCK. Validate the
    // whole set before the first mutation, then every detach below is an
    // invariant-preserving exact-object operation.
    for target in &prepared {
        if !target.parent.is_live()
            || target
                .parent
                .lookup_top(&target.mountpoint)
                .is_none_or(|candidate| !Arc::ptr_eq(&candidate, &target.child))
            || {
                let actual = target
                    .child
                    .children_at(&target.child.mountpoint_root_inode());
                actual.len() != target.root_children_snapshot.len()
                    || actual
                        .iter()
                        .zip(&target.root_children_snapshot)
                        .any(|(actual, expected)| !Arc::ptr_eq(actual, expected))
            }
        {
            return Err(SystemError::EBUSY);
        }
    }
    // Linux propagate_mount_unlock() clears MNT_LOCKED only from the exact
    // corresponding propagation roots before gathering them for umount.  The
    // protected descendants remain locked and, for lazy detach, connected to
    // their detached component.
    for target in &prepared {
        if target.unlock_root {
            target.child.unlock_mount();
        }
    }
    let max_depth = prepared
        .iter()
        .map(|target| target.depth)
        .max()
        .unwrap_or(0);
    for depth in (0..=max_depth).rev() {
        for target in prepared
            .iter_mut()
            .filter(|target| target.disconnect && target.depth == depth)
        {
            let reservation = target
                .reservation
                .as_ref()
                .expect("removable propagated umount target has a reservation");
            target
                .parent
                .detach_exact_restoring_root_children(
                    &target.child,
                    core::mem::take(&mut target.root_children_commit),
                    reservation,
                )
                .expect("validated propagated umount edge commit cannot fail");
        }
    }
    graph.commit_locked();
    for depth in (0..=max_depth).rev() {
        for target in prepared
            .iter()
            .filter(|target| target.disconnect && target.depth == depth)
        {
            target.child.set_self_mountpoint(None);
            MountFS::finish_disconnected_umount(&target.child, lazy)
                .expect("detached prepared propagation root has valid teardown topology");
        }
    }
    Ok(())
}

struct PropagatedUmountTarget {
    parent: Arc<MountFS>,
    mountpoint: Arc<MountFSInode>,
    child: Arc<MountFS>,
    root_children_snapshot: Vec<Arc<MountFS>>,
    /// `(child, restore_to_original_parent)` after deeper commits complete.
    root_children_commit: Vec<(Arc<MountFS>, bool)>,
    nonroot_children: Vec<Arc<MountFS>>,
    marked: bool,
    remove: bool,
    disconnect: bool,
    unlock_root: bool,
    depth: usize,
    reservation: Option<MountEdgeReservation>,
}

fn prepare_propagated_umount_targets(
    sources: &[Arc<MountFS>],
    lazy: bool,
    reserve_commit_capacity: bool,
) -> Result<Vec<PropagatedUmountTarget>, SystemError> {
    let mut result: Vec<PropagatedUmountTarget> = Vec::new();
    let mut candidate_by_id: HashMap<usize, usize> = HashMap::new();
    result
        .try_reserve(sources.len())
        .map_err(|_| SystemError::ENOMEM)?;
    for (depth, source) in sources.iter().rev().enumerate() {
        let source_mountpoint = source.self_mountpoint().ok_or(SystemError::EINVAL)?;
        let parent = source_mountpoint.mount_fs();
        if !parent.propagation().is_shared() {
            continue;
        }
        for target in propagation_targets(&parent) {
            let target_mountpoint = target
                .mount
                .wrapper_for_existing_edge(source_mountpoint.shared_dentry());
            let Some(child) = propagated_child_at(&target.mount, &target_mountpoint) else {
                continue;
            };
            if let Some(index) = candidate_by_id.get(&child.mount_id().data()).copied() {
                let existing = &mut result[index];
                existing.unlock_root |= depth == 0;
                existing.depth = existing.depth.min(depth);
                continue;
            }
            let all_children = child.mount_children();
            let root_children = child.children_at(&child.mountpoint_root_inode());
            let mut nonroot_children = Vec::new();
            nonroot_children
                .try_reserve(all_children.len().saturating_sub(root_children.len()))
                .map_err(|_| SystemError::ENOMEM)?;
            for mounted_child in all_children {
                if !root_children
                    .iter()
                    .any(|root_child| Arc::ptr_eq(root_child, &mounted_child))
                {
                    nonroot_children.push(mounted_child);
                }
            }
            result.try_reserve(1).map_err(|_| SystemError::ENOMEM)?;
            candidate_by_id
                .try_reserve(1)
                .map_err(|_| SystemError::ENOMEM)?;
            let index = result.len();
            candidate_by_id.insert(child.mount_id().data(), index);
            result.push(PropagatedUmountTarget {
                parent: target.mount,
                mountpoint: target_mountpoint,
                child,
                root_children_snapshot: root_children,
                root_children_commit: Vec::new(),
                nonroot_children,
                marked: false,
                remove: false,
                disconnect: false,
                unlock_root: depth == 0,
                depth,
                reservation: None,
            });
        }
    }

    // Linux's repeated __propagate_umount(parent) walk is a fixed point. Use
    // reverse dependencies so deep hostile trees remain O(V + E) while the
    // global topology lock is held.
    let mut dependents = Vec::new();
    dependents
        .try_reserve(result.len())
        .map_err(|_| SystemError::ENOMEM)?;
    dependents.resize_with(result.len(), Vec::new);
    let mut remaining = Vec::new();
    remaining
        .try_reserve(result.len())
        .map_err(|_| SystemError::ENOMEM)?;
    remaining.resize(result.len(), 0usize);
    let mut blocked = Vec::new();
    blocked
        .try_reserve(result.len())
        .map_err(|_| SystemError::ENOMEM)?;
    blocked.resize(result.len(), false);
    for (index, target) in result.iter().enumerate() {
        for child in &target.nonroot_children {
            let Some(child_index) = candidate_by_id.get(&child.mount_id().data()).copied() else {
                blocked[index] = true;
                continue;
            };
            dependents[child_index]
                .try_reserve(1)
                .map_err(|_| SystemError::ENOMEM)?;
            dependents[child_index].push(index);
            remaining[index] += 1;
        }
    }
    let mut ready = VecDeque::new();
    ready
        .try_reserve(result.len())
        .map_err(|_| SystemError::ENOMEM)?;
    for index in 0..result.len() {
        if !blocked[index] && remaining[index] == 0 {
            ready.push_back(index);
        }
    }
    while let Some(index) = ready.pop_front() {
        if result[index].marked {
            continue;
        }
        result[index].marked = true;
        for dependent in &dependents[index] {
            remaining[*dependent] -= 1;
            if !blocked[*dependent] && remaining[*dependent] == 0 {
                ready.push_back(*dependent);
            }
        }
    }
    for target in &mut result {
        target.remove = target.marked && (target.unlock_root || !target.child.is_locked());
    }
    // umount_list() includes marked locked children once a selected ancestor
    // is removed, while a locked root without such an ancestor is restored.
    let mut children_by_parent = Vec::new();
    children_by_parent
        .try_reserve(result.len())
        .map_err(|_| SystemError::ENOMEM)?;
    children_by_parent.resize_with(result.len(), Vec::new);
    for (index, target) in result.iter().enumerate() {
        if let Some(parent_index) = candidate_by_id
            .get(&target.parent.mount_id().data())
            .copied()
            .filter(|parent_index| {
                !Arc::ptr_eq(
                    &target.mountpoint.shared_dentry(),
                    &result[*parent_index].child.root_dentry(),
                )
            })
        {
            children_by_parent[parent_index]
                .try_reserve(1)
                .map_err(|_| SystemError::ENOMEM)?;
            children_by_parent[parent_index].push(index);
        }
    }
    let mut removed = VecDeque::new();
    removed
        .try_reserve(result.len())
        .map_err(|_| SystemError::ENOMEM)?;
    for (index, target) in result.iter().enumerate() {
        if target.remove {
            removed.push_back(index);
        }
    }
    while let Some(parent_index) = removed.pop_front() {
        for child_index in &children_by_parent[parent_index] {
            if result[*child_index].marked && !result[*child_index].remove {
                result[*child_index].remove = true;
                removed.push_back(*child_index);
            }
        }
    }

    // Linux disconnect_mount() retains a locked selected child below a
    // selected parent during lazy detach. It still leaves the namespace and
    // propagation graph, but the highest disconnected ancestor owns teardown.
    for index in 0..result.len() {
        let parent_removed = candidate_by_id
            .get(&result[index].parent.mount_id().data())
            .is_some_and(|parent| result[*parent].remove);
        result[index].disconnect = result[index].remove
            && !(lazy
                && !result[index].unlock_root
                && result[index].child.is_locked()
                && parent_removed);
    }

    // Once the closure is final, distinguish already-disconnected root
    // children, lazy-locked retained children, and covers that must be restored.
    for index in 0..result.len() {
        let mut commit = Vec::new();
        commit
            .try_reserve(result[index].root_children_snapshot.len())
            .map_err(|_| SystemError::ENOMEM)?;
        for child in &result[index].root_children_snapshot {
            match candidate_by_id.get(&child.mount_id().data()).copied() {
                Some(child_index) if result[child_index].disconnect => {}
                Some(child_index) if result[child_index].remove => {
                    commit.push((child.clone(), false));
                }
                _ => commit.push((child.clone(), true)),
            }
        }
        result[index].root_children_commit = commit;
    }
    if reserve_commit_capacity {
        for target in result.iter_mut().filter(|target| target.disconnect) {
            let restore_count = target
                .root_children_commit
                .iter()
                .filter(|(_, restore)| *restore)
                .count();
            target.reservation = Some(
                target
                    .parent
                    .reserve_mount_edge(&target.mountpoint, restore_count.saturating_sub(1))?,
            );
        }
    }
    Ok(result)
}

fn propagated_child_at(
    parent: &Arc<MountFS>,
    mountpoint: &Arc<MountFSInode>,
) -> Option<Arc<MountFS>> {
    // Linux propagation is keyed only by `(parent mount, mountpoint dentry)`.
    // The child's propagation type may have changed independently after the
    // original mount event and therefore cannot be used as correspondence.
    // Linux __lookup_mnt() returns the first mount-hash entry. New mounts are
    // inserted at the hash head, so that entry is the visible topper. DragonOS
    // stores stacks oldest-to-newest and therefore represents the same rule as
    // Vec::last() through lookup_top().
    parent.lookup_top(mountpoint)
}

/// Preflight the propagation set before ordinary umount mutates topology.
pub fn propagation_umount_busy(parent_mnt: &Arc<MountFS>, mountpoint: &Arc<MountFSInode>) -> bool {
    let propagation = parent_mnt.propagation();
    if !propagation.is_shared() {
        return false;
    }
    let Some(source_child) = parent_mnt.lookup_top(mountpoint) else {
        return true;
    };
    prepare_propagated_umount_targets(core::slice::from_ref(&source_child), false, false)
        .map(|targets| {
            targets.into_iter().any(|target| {
                // Linux checks the candidate refcount only when it has no
                // children or one root topper. Other child layouts make
                // the propagation branch non-busy (and either restore the
                // candidate or restore its complete root stack later).
                target.remove
                    && target.root_children_snapshot.len() <= 1
                    && target.child.has_external_pins()
            })
        })
        .unwrap_or(true)
}

/// Umount at a specific peer mount.
///
/// Does NOT call `sync_filesystem()` here: all propagation clones share the same
/// `super_block_state` (including `umount_lock`) via `deepcopy()`. The top-level
/// `umount()` already holds the write lock while running the sync body; syncing
/// again here would be redundant and cause a RwSem self-deadlock.
#[cfg(test)]
pub(super) fn umount_at_peer(
    peer_mnt: &Arc<MountFS>,
    source_mountpoint: &Arc<MountFSInode>,
    _source_child: &Arc<MountFS>,
) -> Result<(), SystemError> {
    let peer_mountpoint = match peer_mnt.wrapper_for_dentry(source_mountpoint.shared_dentry()) {
        Ok(mountpoint) => mountpoint,
        Err(SystemError::EXDEV) => return Ok(()),
        Err(error) => return Err(error),
    };
    let Some(child) = propagated_child_at(peer_mnt, &peer_mountpoint) else {
        return Ok(());
    };
    peer_mnt.detach_exact(&child)?;
    MountFS::deactivate_disconnected_subtree(&child);

    Ok(())
}
