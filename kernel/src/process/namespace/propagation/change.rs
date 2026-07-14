//! Atomic propagation-type changes for one mount or a recursive mount subtree.

use alloc::sync::Arc;
use alloc::vec::Vec;

use hashbrown::HashMap;
use system_error::SystemError;

use crate::filesystem::vfs::{
    mount::{MountFlags, MountId, MOUNT_LIFECYCLE_LOCK},
    MountFS,
};
use crate::libs::mutex::MutexGuard;

use super::group::{
    apply_prepared_peer_groups, count_new_peer_group_keys, try_reserve_peer_group_keys,
    try_snapshot_peer_group, PreparedPeerGroupState, PropagationGroup, PropagationGroupId,
};
use super::state::{PreparedMountPropagationState, PropagationFlags, PropagationType};

type GraphMountId = MountId;

/// Convert mount flags to propagation type.
///
/// Linux removes only `MS_REC | MS_SILENT`, rejects every remaining
/// non-propagation bit, and requires exactly one propagation type bit.
pub fn flags_to_propagation_type(flags: MountFlags) -> Result<PropagationType, SystemError> {
    let propagation_mask =
        MountFlags::SHARED | MountFlags::SLAVE | MountFlags::PRIVATE | MountFlags::UNBINDABLE;
    let allowed = propagation_mask | MountFlags::REC | MountFlags::SILENT;
    if !(flags & !allowed).is_empty() {
        return Err(SystemError::EINVAL);
    }

    let type_flags = flags & propagation_mask;
    if type_flags.bits().count_ones() != 1 {
        return Err(SystemError::EINVAL);
    }

    if type_flags == MountFlags::SHARED {
        Ok(PropagationType::Shared)
    } else if type_flags == MountFlags::SLAVE {
        Ok(PropagationType::Slave)
    } else if type_flags == MountFlags::PRIVATE {
        Ok(PropagationType::Private)
    } else {
        Ok(PropagationType::Unbindable)
    }
}

/// Check if mount flags indicate a propagation type change request.
pub fn is_propagation_change(flags: MountFlags) -> bool {
    flags.intersects(
        MountFlags::SHARED | MountFlags::PRIVATE | MountFlags::SLAVE | MountFlags::UNBINDABLE,
    )
}

/// Change the propagation type of one mount.
#[cfg(test)]
pub(super) fn change_mnt_propagation(
    mount: &Arc<MountFS>,
    prop_type: PropagationType,
) -> Result<(), SystemError> {
    change_mnt_propagation_recursive(mount, prop_type, false)
}

struct PropagationGraphNode {
    mount: Arc<MountFS>,
    flags: PropagationFlags,
    peer_group: Option<Arc<PropagationGroup>>,
    master: Option<GraphMountId>,
    slaves: Vec<GraphMountId>,
}

struct PropagationGraphGroup {
    members: Vec<GraphMountId>,
}

struct PropagationGraph {
    nodes: HashMap<GraphMountId, PropagationGraphNode>,
    order: Vec<GraphMountId>,
    groups: HashMap<usize, PropagationGraphGroup>,
}

struct PreparedMountStateUpdate {
    mount: Arc<MountFS>,
    state: PreparedMountPropagationState,
}

pub(super) struct PropagationChangeTransaction {
    _topology_guard: MutexGuard<'static, ()>,
    mount_states: HashMap<GraphMountId, PreparedMountStateUpdate>,
    peer_groups: Vec<PreparedPeerGroupState>,
}

fn reserve_vec<T, R>(
    vec: &mut Vec<T>,
    additional: usize,
    before_reserve: &mut R,
) -> Result<(), SystemError>
where
    R: FnMut() -> Result<(), SystemError>,
{
    if additional == 0 {
        return Ok(());
    }
    before_reserve()?;
    vec.try_reserve(additional).map_err(|_| SystemError::ENOMEM)
}

fn reserve_map<K, V, R>(
    map: &mut HashMap<K, V>,
    additional: usize,
    before_reserve: &mut R,
) -> Result<(), SystemError>
where
    K: core::hash::Hash + Eq,
    R: FnMut() -> Result<(), SystemError>,
{
    if additional == 0 {
        return Ok(());
    }
    before_reserve()?;
    map.try_reserve(additional).map_err(|_| SystemError::ENOMEM)
}

impl PropagationGraph {
    fn new<R>(target_count: usize, before_reserve: &mut R) -> Result<Self, SystemError>
    where
        R: FnMut() -> Result<(), SystemError>,
    {
        let mut nodes = HashMap::new();
        reserve_map(&mut nodes, target_count, before_reserve)?;
        let mut order = Vec::new();
        reserve_vec(&mut order, target_count, before_reserve)?;
        let mut groups = HashMap::new();
        reserve_map(&mut groups, target_count, before_reserve)?;
        Ok(Self {
            nodes,
            order,
            groups,
        })
    }

    /// Capture the complete master/slave component reachable from `seed`.
    /// Rebuilding reverse slave vectors is safe only when every existing edge
    /// incident on a replaced master is represented by its forward master edge.
    fn capture_component<R>(
        &mut self,
        seed: Arc<MountFS>,
        before_reserve: &mut R,
    ) -> Result<(), SystemError>
    where
        R: FnMut() -> Result<(), SystemError>,
    {
        let mut pending = Vec::new();
        reserve_vec(&mut pending, 1, before_reserve)?;
        pending.push(seed);
        let mut index = 0;
        while index < pending.len() {
            let mount = pending[index].clone();
            index += 1;
            let id = mount.mount_id();
            if self.nodes.contains_key(&id) {
                continue;
            }

            let snapshot = mount.propagation().try_snapshot_for_graph(before_reserve)?;
            let master = snapshot.master;
            let candidate_slaves = snapshot.candidate_slaves;

            // A reverse entry is authoritative only when the slave's forward
            // edge still names this mount. Check after dropping this mount's
            // spin lock so malformed legacy state cannot create nested-lock
            // deadlocks while the transaction normalizes stale weak entries.
            let mut slaves = Vec::new();
            reserve_vec(&mut slaves, candidate_slaves.len(), before_reserve)?;
            for slave in candidate_slaves {
                if slave
                    .propagation()
                    .master()
                    .is_some_and(|slave_master| Arc::ptr_eq(&slave_master, &mount))
                {
                    slaves.push(slave);
                }
            }
            let mut slave_ids = Vec::new();
            reserve_vec(&mut slave_ids, slaves.len(), before_reserve)?;
            slave_ids.extend(slaves.iter().map(|slave| slave.mount_id()));
            let node = PropagationGraphNode {
                mount: mount.clone(),
                flags: snapshot.flags,
                peer_group: snapshot.peer_group,
                master: master.as_ref().map(|master| master.mount_id()),
                slaves: slave_ids,
            };

            reserve_map(&mut self.nodes, 1, before_reserve)?;
            reserve_vec(&mut self.order, 1, before_reserve)?;
            self.nodes.insert(id, node);
            self.order.push(id);

            let additional = usize::from(master.is_some()) + slaves.len();
            reserve_vec(&mut pending, additional, before_reserve)?;
            if let Some(master) = master {
                pending.push(master);
            }
            pending.extend(slaves);
        }
        Ok(())
    }

    /// Snapshot one touched peer group in registry order. Every peer's full
    /// master/slave component is captured because a peer may become the new
    /// master and its unrelated existing slaves must survive final-state replace.
    fn capture_group<R>(
        &mut self,
        group_id: PropagationGroupId,
        required_member: &Arc<MountFS>,
        before_reserve: &mut R,
    ) -> Result<(), SystemError>
    where
        R: FnMut() -> Result<(), SystemError>,
    {
        if self.groups.contains_key(&group_id.data()) {
            return Ok(());
        }

        let mut members = try_snapshot_peer_group(group_id, before_reserve)?;
        if !members
            .iter()
            .any(|member| Arc::ptr_eq(member, required_member))
        {
            reserve_vec(&mut members, 1, before_reserve)?;
            members.push(required_member.clone());
        }

        let mut member_ids = Vec::new();
        reserve_vec(&mut member_ids, members.len(), before_reserve)?;
        for member in members {
            self.capture_component(member.clone(), before_reserve)?;
            member_ids.push(member.mount_id());
        }
        reserve_map(&mut self.groups, 1, before_reserve)?;
        self.groups.insert(
            group_id.data(),
            PropagationGraphGroup {
                members: member_ids,
            },
        );
        Ok(())
    }

    fn simulate_make_slave<R>(
        &mut self,
        target: GraphMountId,
        before_reserve: &mut R,
    ) -> Result<(), SystemError>
    where
        R: FnMut() -> Result<(), SystemError>,
    {
        let (was_shared, old_group_id, old_master, target_root) = {
            let node = self.nodes.get(&target).unwrap();
            (
                node.flags.contains(PropagationFlags::SHARED),
                node.peer_group.as_ref().map(|group| group.id().data()),
                node.master,
                node.mount.root_dentry(),
            )
        };

        let master = if was_shared {
            let group_id = old_group_id.expect("shared graph node has a group");
            let members = &self.groups.get(&group_id).unwrap().members;
            let target_index = members
                .iter()
                .position(|member| *member == target)
                .expect("shared graph group contains its target");
            let mut fallback = None;
            let mut exact = None;
            for offset in 1..members.len() {
                let member = members[(target_index + offset) % members.len()];
                fallback.get_or_insert(member);
                if Arc::ptr_eq(
                    &self.nodes.get(&member).unwrap().mount.root_dentry(),
                    &target_root,
                ) {
                    exact = Some(member);
                    break;
                }
            }
            let master = exact.or(fallback).or(old_master);
            self.groups
                .get_mut(&group_id)
                .unwrap()
                .members
                .retain(|member| *member != target);
            let node = self.nodes.get_mut(&target).unwrap();
            node.flags.remove(PropagationFlags::SHARED);
            node.peer_group = None;
            master
        } else {
            old_master
        };

        let migrating_slaves = core::mem::take(&mut self.nodes.get_mut(&target).unwrap().slaves);
        if let Some(old_master) = old_master {
            self.nodes
                .get_mut(&old_master)
                .unwrap()
                .slaves
                .retain(|slave| *slave != target);
        }
        if let Some(master) = master {
            let master_slaves = &mut self.nodes.get_mut(&master).unwrap().slaves;
            let additional = migrating_slaves
                .len()
                .checked_add(1)
                .ok_or(SystemError::ENOMEM)?;
            reserve_vec(master_slaves, additional, before_reserve)?;
            // Linux list_move() puts the converted mount at the head of the
            // new master's slave list, then appends its migrating slaves.
            master_slaves.insert(0, target);
            master_slaves.extend(migrating_slaves.iter().copied());
            for slave in migrating_slaves {
                self.nodes.get_mut(&slave).unwrap().master = Some(master);
            }
        } else {
            for slave in migrating_slaves {
                self.nodes.get_mut(&slave).unwrap().master = None;
            }
        }
        self.nodes.get_mut(&target).unwrap().master = master;
        Ok(())
    }

    fn detach_graph_master(&mut self, target: GraphMountId) {
        if let Some(master) = self.nodes.get(&target).unwrap().master {
            self.nodes
                .get_mut(&master)
                .unwrap()
                .slaves
                .retain(|slave| *slave != target);
        }
        self.nodes.get_mut(&target).unwrap().master = None;
    }

    fn simulate_change<A, R>(
        &mut self,
        targets: &[GraphMountId],
        prop_type: PropagationType,
        alloc_group: &mut A,
        before_reserve: &mut R,
    ) -> Result<(), SystemError>
    where
        A: FnMut() -> Result<Arc<PropagationGroup>, SystemError>,
        R: FnMut() -> Result<(), SystemError>,
    {
        for target in targets {
            match prop_type {
                PropagationType::Shared => {
                    if !self
                        .nodes
                        .get(target)
                        .unwrap()
                        .flags
                        .contains(PropagationFlags::SHARED)
                    {
                        let group = alloc_group()?;
                        let group_id = group.id().data();
                        let node = self.nodes.get_mut(target).unwrap();
                        node.flags.remove(PropagationFlags::UNBINDABLE);
                        node.flags.insert(PropagationFlags::SHARED);
                        node.peer_group = Some(group);
                        let mut members = Vec::new();
                        reserve_vec(&mut members, 1, before_reserve)?;
                        members.push(*target);
                        reserve_map(&mut self.groups, 1, before_reserve)?;
                        self.groups
                            .insert(group_id, PropagationGraphGroup { members });
                    }
                }
                PropagationType::Slave => {
                    self.simulate_make_slave(*target, before_reserve)?;
                }
                PropagationType::Private => {
                    self.simulate_make_slave(*target, before_reserve)?;
                    self.detach_graph_master(*target);
                    let node = self.nodes.get_mut(target).unwrap();
                    node.flags
                        .remove(PropagationFlags::SHARED | PropagationFlags::UNBINDABLE);
                    node.peer_group = None;
                }
                PropagationType::Unbindable => {
                    self.simulate_make_slave(*target, before_reserve)?;
                    self.detach_graph_master(*target);
                    let node = self.nodes.get_mut(target).unwrap();
                    node.flags.remove(PropagationFlags::SHARED);
                    node.flags.insert(PropagationFlags::UNBINDABLE);
                    node.peer_group = None;
                }
            }
        }
        Ok(())
    }
}

pub(super) fn collect_change_targets<R>(
    root: &Arc<MountFS>,
    recursive: bool,
    before_reserve: &mut R,
) -> Result<Vec<Arc<MountFS>>, SystemError>
where
    R: FnMut() -> Result<(), SystemError>,
{
    let mut targets = Vec::new();
    let mut stack = Vec::new();
    reserve_vec(&mut stack, 1, before_reserve)?;
    stack.push(root.clone());
    while let Some(current) = stack.pop() {
        reserve_vec(&mut targets, 1, before_reserve)?;
        targets.push(current.clone());
        if !recursive {
            continue;
        }

        let mountpoints = current.mountpoints();
        let child_count = mountpoints.values().map(Vec::len).sum();
        let mut children = Vec::new();
        reserve_vec(&mut children, child_count, before_reserve)?;
        for shadow_stack in mountpoints.values() {
            children.extend(shadow_stack.iter().cloned());
        }
        drop(mountpoints);
        reserve_vec(&mut stack, children.len(), before_reserve)?;
        stack.extend(children.into_iter().rev());
    }
    Ok(targets)
}

impl PropagationChangeTransaction {
    pub(super) fn prepare<A, R>(
        root: &Arc<MountFS>,
        prop_type: PropagationType,
        recursive: bool,
        mut alloc_group: A,
        mut before_reserve: R,
    ) -> Result<Self, SystemError>
    where
        A: FnMut() -> Result<Arc<PropagationGroup>, SystemError>,
        R: FnMut() -> Result<(), SystemError>,
    {
        let topology_guard = MOUNT_LIFECYCLE_LOCK.lock();
        let targets = collect_change_targets(root, recursive, &mut before_reserve)?;
        for target in &targets {
            if !target.is_live() {
                return Err(SystemError::EINVAL);
            }
        }

        let mut graph = PropagationGraph::new(targets.len(), &mut before_reserve)?;
        for target in &targets {
            graph.capture_component(target.clone(), &mut before_reserve)?;
        }

        // Snapshot every initially touched group before simulation changes any
        // graph node; registry filtering must observe the real pre-transaction state.
        for target in &targets {
            let propagation = target.propagation();
            if propagation.is_shared() {
                graph.capture_group(propagation.peer_group_id(), target, &mut before_reserve)?;
            }
        }

        let mut target_ids = Vec::new();
        reserve_vec(&mut target_ids, targets.len(), &mut before_reserve)?;
        target_ids.extend(targets.iter().map(|target| target.mount_id()));
        graph.simulate_change(
            &target_ids,
            prop_type,
            &mut alloc_group,
            &mut before_reserve,
        )?;

        let mut mount_states = HashMap::new();
        reserve_map(&mut mount_states, graph.nodes.len(), &mut before_reserve)?;
        for id in &graph.order {
            let node = graph.nodes.get(id).unwrap();
            let mut slaves = Vec::new();
            reserve_vec(&mut slaves, node.slaves.len(), &mut before_reserve)?;
            slaves.extend(
                node.slaves
                    .iter()
                    .map(|slave| Arc::downgrade(&graph.nodes.get(slave).unwrap().mount)),
            );
            let master = node
                .master
                .map(|master| Arc::downgrade(&graph.nodes.get(&master).unwrap().mount));
            mount_states.insert(
                *id,
                PreparedMountStateUpdate {
                    mount: node.mount.clone(),
                    state: PreparedMountPropagationState {
                        flags: node.flags,
                        peer_group: node.peer_group.clone(),
                        master,
                        slaves,
                    },
                },
            );
        }
        let mut peer_groups = Vec::new();
        reserve_vec(&mut peer_groups, graph.groups.len(), &mut before_reserve)?;
        for (group_id, group) in graph.groups {
            if group.members.is_empty() {
                peer_groups.push(PreparedPeerGroupState::Remove(group_id));
                continue;
            }
            let mut members = Vec::new();
            reserve_vec(&mut members, group.members.len(), &mut before_reserve)?;
            members.extend(
                group
                    .members
                    .iter()
                    .map(|member| Arc::downgrade(&graph.nodes.get(member).unwrap().mount)),
            );
            peer_groups.push(PreparedPeerGroupState::Replace(group_id, members));
        }
        let new_group_keys = count_new_peer_group_keys(&peer_groups);
        try_reserve_peer_group_keys(new_group_keys, &mut before_reserve)?;

        // Drop every snapshot-only owner while the topology guard is still
        // held. The returned transaction owns only final state and resources.
        drop(graph.nodes);
        drop(graph.order);
        drop(target_ids);
        drop(targets);

        Ok(Self {
            _topology_guard: topology_guard,
            mount_states,
            peer_groups,
        })
    }

    fn commit(self) {
        let Self {
            _topology_guard,
            mount_states,
            peer_groups,
        } = self;
        apply_prepared_peer_groups(peer_groups);
        for (_, state) in mount_states {
            let old_state = state.mount.propagation().replace_state(state.state);
            // Release a last group owner after the per-mount spin lock. IDA
            // removal and the lowest-free cursor update never allocate.
            drop(old_state);
        }
        drop(_topology_guard);
    }
}

/// Change the propagation type of a mount tree (recursive).
///
/// This implements `mount --make-r{shared,private,slave,unbindable}`.
///
/// # Arguments
/// * `mount` - The root mount of the subtree
/// * `prop_type` - The new propagation type
/// * `recursive` - Whether to apply recursively
///
/// # Returns
/// * `Ok(())` on success
/// * `Err(SystemError)` on failure
pub fn change_mnt_propagation_recursive(
    mount: &Arc<MountFS>,
    prop_type: PropagationType,
    recursive: bool,
) -> Result<(), SystemError> {
    let transaction = PropagationChangeTransaction::prepare(
        mount,
        prop_type,
        recursive,
        PropagationGroup::alloc,
        || Ok(()),
    )?;
    transaction.commit();
    Ok(())
}
