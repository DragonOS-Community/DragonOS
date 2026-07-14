//! Mount, move, and unmount event propagation across peer/slave topology.

use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;

use hashbrown::HashSet;
use system_error::SystemError;

use crate::filesystem::vfs::{mount::MountFSInode, MountFS};

use super::group::{get_peers, register_peer, unregister_peer, PropagationGroup};
use super::state::{register_slave_with_master, PropagationType};

#[derive(Clone, Copy)]
enum PropagationTargetKind {
    Peer,
    Slave,
}

struct PropagationTarget {
    mount: Arc<MountFS>,
    kind: PropagationTargetKind,
    master_parent: Option<Arc<MountFS>>,
}

struct PreparedMount {
    target_parent: Arc<MountFS>,
    mountpoint: Arc<MountFSInode>,
    expected_top: Option<Arc<MountFS>>,
    clone: Arc<MountFS>,
}

pub(crate) struct PreparedPropagation {
    source_mnt: Arc<MountFS>,
    mountpoint: Arc<MountFSInode>,
    new_child: Arc<MountFS>,
    mounts: Vec<PreparedMount>,
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
                master_parent: None,
            });
        }
    }

    let mut pending: Vec<(Arc<MountFS>, Arc<MountFS>)> = source
        .propagation()
        .slaves()
        .into_iter()
        .map(|slave| (slave, source.clone()))
        .collect();
    for peer in result.iter().map(|target| &target.mount) {
        pending.extend(
            peer.propagation()
                .slaves()
                .into_iter()
                .map(|slave| (slave, peer.clone())),
        );
    }
    while let Some((slave, master_parent)) = pending.pop() {
        if !visited.insert(slave.mount_id().data()) {
            continue;
        }
        pending.extend(
            slave
                .propagation()
                .slaves()
                .into_iter()
                .map(|child| (child, slave.clone())),
        );
        result.push(PropagationTarget {
            mount: slave,
            kind: PropagationTargetKind::Slave,
            master_parent: Some(master_parent),
        });
    }
    result
}

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

fn register_subtree(root: &Arc<MountFS>) {
    for mount in collect_subtree(root) {
        let prop = mount.propagation();
        if prop.is_shared() {
            register_peer(prop.peer_group_id(), &mount);
        }
        register_slave_with_master(&mount);
    }
}

pub(crate) fn prepare_mount_propagation_locked(
    source_mnt: &Arc<MountFS>,
    mountpoint: &Arc<MountFSInode>,
    new_child: &Arc<MountFS>,
) -> Result<Option<PreparedPropagation>, SystemError> {
    let source_prop = source_mnt.propagation();
    if !source_prop.is_shared() {
        return Ok(None);
    }
    let canonical_mountpoint = source_mnt.wrapper_for_dentry(mountpoint.shared_dentry())?;
    if canonical_mountpoint.dentry_id() != mountpoint.dentry_id() {
        return Err(SystemError::EINVAL);
    }

    let source_dentry = mountpoint.shared_dentry();
    let mut slave_groups = BTreeMap::new();
    let mut mounts = Vec::new();
    let mut propagated_sources = BTreeMap::new();
    propagated_sources.insert(source_mnt.mount_id().data(), new_child.clone());
    for target in propagation_targets(source_mnt) {
        let PropagationTarget {
            mount: target_parent,
            kind,
            master_parent,
        } = target;
        let master_source = master_parent
            .as_ref()
            .and_then(|master| propagated_sources.get(&master.mount_id().data()).cloned());
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
        propagated_sources.insert(target_parent.mount_id().data(), clone.clone());
        mounts.push(PreparedMount {
            target_parent,
            mountpoint: target_mp,
            expected_top,
            clone,
        });
    }
    Ok(Some(PreparedPropagation {
        source_mnt: source_mnt.clone(),
        mountpoint: canonical_mountpoint,
        new_child: new_child.clone(),
        mounts,
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
    for item in &prepared.mounts {
        let result = if item.expected_top.is_some() {
            item.target_parent
                .attach_beneath(&item.mountpoint, item.clone.clone())
        } else {
            item.target_parent
                .attach_top(&item.mountpoint, item.clone.clone())
        };
        if let Err(error) = result {
            for committed in attached.iter().rev() {
                let _: Result<Arc<MountFS>, _> = committed
                    .target_parent
                    .detach_exact_restoring_cover(&committed.clone);
            }
            abandon_prepared(&prepared.mounts);
            return Err(error);
        }
        attached.push(item);
    }

    for item in &prepared.mounts {
        register_subtree(&item.clone);
    }
    Ok(())
}

/// Linux makes every mount in a moved tree shared before propagating the tree
/// into the destination parent's peers.  The complete tree is copied once per
/// destination instead of the former path/BFS reconstruction.
pub(crate) fn propagate_moved_tree_locked(
    target_parent: &Arc<MountFS>,
    moved_root: &Arc<MountFS>,
    moved_root_mountpoint: &Arc<MountFSInode>,
) -> Result<(), SystemError> {
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
            register_peer(prop.peer_group_id(), mount);
        }
    }
    let propagation =
        prepare_mount_propagation_locked(target_parent, moved_root_mountpoint, moved_root);
    if let Err(error) = propagation.and_then(commit_mount_propagation_locked) {
        // Linux discards invented group IDs when propagation preparation
        // fails. Restore the pre-move state rather than leaking a semantic
        // change from a failed move.
        for (mount, old_type, _) in invented.into_iter().rev() {
            let prop = mount.propagation();
            unregister_peer(prop.peer_group_id(), &mount);
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
        return Err(error);
    }
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
pub fn propagate_umount(
    parent_mnt: &Arc<MountFS>,
    mountpoint: &Arc<MountFSInode>,
    source_child: &Arc<MountFS>,
    lazy: bool,
) -> Result<(), SystemError> {
    let propagation = parent_mnt.propagation();

    // Only propagate for shared mounts
    if !propagation.is_shared() {
        return Ok(());
    }

    // log::debug!(
    //     "propagate_umount: propagating umount from group {} to peers",
    //     group_id.0
    // );

    let prepared = propagated_umount_targets(parent_mnt, mountpoint, source_child)?;

    // The caller serializes detach through MOUNT_LIFECYCLE_LOCK. Validate the
    // whole set before the first mutation, then every detach below is an
    // invariant-preserving exact-object operation.
    for (target, target_mountpoint, child) in &prepared {
        if !target.is_live()
            || !target
                .children_at(target_mountpoint)
                .iter()
                .any(|candidate| Arc::ptr_eq(candidate, child))
        {
            return Err(SystemError::EBUSY);
        }
    }
    // Linux propagate_mount_unlock() clears MNT_LOCKED only from the exact
    // corresponding propagation roots before gathering them for umount.  The
    // protected descendants remain locked and, for lazy detach, connected to
    // their detached component.
    for (_, _, child) in &prepared {
        child.unlock_mount();
    }
    for (target, _, child) in &prepared {
        target.detach_exact_restoring_cover(child)?;
    }
    for (_, _, child) in prepared {
        child.set_self_mountpoint(None);
        MountFS::finish_disconnected_umount(&child, lazy)?;
    }
    Ok(())
}

type PropagatedUmountTarget = (Arc<MountFS>, Arc<MountFSInode>, Arc<MountFS>);

fn propagated_umount_targets(
    parent_mnt: &Arc<MountFS>,
    mountpoint: &Arc<MountFSInode>,
    source_child: &Arc<MountFS>,
) -> Result<Vec<PropagatedUmountTarget>, SystemError> {
    // A deep slave's child is mastered by the corresponding child in the
    // immediately preceding layer, not by the original source child. Keep the
    // same parent->child projection that mount propagation uses.
    let mut corresponding = BTreeMap::new();
    corresponding.insert(parent_mnt.mount_id().data(), source_child.clone());
    let mut result = Vec::new();
    for target in propagation_targets(parent_mnt) {
        let reference_child = match target.kind {
            PropagationTargetKind::Peer => source_child.clone(),
            PropagationTargetKind::Slave => {
                let Some(master_parent) = target.master_parent.as_ref() else {
                    continue;
                };
                let Some(child) = corresponding.get(&master_parent.mount_id().data()) else {
                    continue;
                };
                child.clone()
            }
        };
        let target_mountpoint = match target.mount.wrapper_for_dentry(mountpoint.shared_dentry()) {
            Ok(mountpoint) => mountpoint,
            Err(SystemError::EXDEV) => continue,
            Err(error) => return Err(error),
        };
        let Some(child) = propagated_child_at(&target.mount, &target_mountpoint, &reference_child)
        else {
            continue;
        };
        corresponding.insert(target.mount.mount_id().data(), child.clone());
        result.push((target.mount, target_mountpoint, child));
    }
    Ok(result)
}

fn propagated_child_at(
    parent: &Arc<MountFS>,
    mountpoint: &Arc<MountFSInode>,
    source_child: &Arc<MountFS>,
) -> Option<Arc<MountFS>> {
    let source_prop = source_child.propagation();
    parent
        .children_at(mountpoint)
        .into_iter()
        .rev()
        .find(|candidate| {
            let candidate_prop = candidate.propagation();
            (source_prop.is_shared()
                && candidate_prop.is_shared()
                && candidate_prop.peer_group_id() == source_prop.peer_group_id())
                || candidate_prop
                    .master()
                    .is_some_and(|master| Arc::ptr_eq(&master, source_child))
        })
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
    propagated_umount_targets(parent_mnt, mountpoint, &source_child)
        .map(|targets| {
            targets.into_iter().any(|(_, _, child)| {
                !child.mount_children().is_empty() || child.subtree_has_external_pins()
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
    source_child: &Arc<MountFS>,
) -> Result<(), SystemError> {
    let peer_mountpoint = match peer_mnt.wrapper_for_dentry(source_mountpoint.shared_dentry()) {
        Ok(mountpoint) => mountpoint,
        Err(SystemError::EXDEV) => return Ok(()),
        Err(error) => return Err(error),
    };
    let Some(child) = propagated_child_at(peer_mnt, &peer_mountpoint, source_child) else {
        return Ok(());
    };
    peer_mnt.detach_exact(&child)?;
    MountFS::deactivate_disconnected_subtree(&child);

    Ok(())
}
