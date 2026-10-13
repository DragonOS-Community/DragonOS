//! CPU controller control plane. All topology/configuration callers hold the
//! existing cgroup update transaction; scheduler hot paths use stable PCB refs.
pub(crate) mod accounting;

use alloc::{sync::Arc, vec::Vec};
use system_error::SystemError;

use crate::{
    process::ProcessControlBlock,
    sched::{fair_group, LoadWeight, TaskGroup},
};

use super::{CgroupCpuState, CgroupNode};

/// Disabled nodes inherit the nearest enabled CPU css, not their resource
/// domain: CPU is a threaded controller and each enabled leaf is independent.
pub(crate) fn effective_group(node: &Arc<CgroupNode>) -> Option<Arc<TaskGroup>> {
    let mut cursor = Some(node.clone());
    while let Some(node) = cursor {
        if let Some(group) = node.cpu_group() {
            return Some(group);
        }
        cursor = node.parent();
    }
    None
}

pub(crate) fn initialize_node_locked(node: &Arc<CgroupNode>) -> Result<(), SystemError> {
    let Some(parent) = node.parent() else {
        return Ok(());
    };
    if parent.subtree_controller_enabled("cpu") {
        let parent_group = effective_group(&parent);
        let group = TaskGroup::try_new(parent_group.clone())?;
        TaskGroup::reserve_parent_children(&parent_group, 1)?;
        group.publish_parent();
        node.set_cpu_group(Some(group));
    }
    Ok(())
}

pub(crate) struct PreparedCpuControl {
    enabled: bool,
    groups: Vec<(Arc<CgroupNode>, Option<Arc<TaskGroup>>)>,
}

/// Preallocate unpublished css instances before changing the controller mask.
/// A folded no-op may discard them, but cannot publish partial topology.
pub(crate) fn prepare_control_locked(
    node: &Arc<CgroupNode>,
    enabled: bool,
) -> Result<PreparedCpuControl, SystemError> {
    let parent = effective_group(node);
    let children = node.children_snapshot()?;
    let mut groups = Vec::new();
    groups
        .try_reserve_exact(children.len())
        .map_err(|_| SystemError::ENOMEM)?;
    for child in children {
        let group = if enabled {
            Some(TaskGroup::try_new(parent.clone())?)
        } else {
            child.cpu_group()
        };
        groups.push((child, group));
    }
    if enabled {
        TaskGroup::reserve_parent_children(&parent, groups.len())?;
    }
    Ok(PreparedCpuControl { enabled, groups })
}

pub(crate) fn controller_changed_locked(
    node: &Arc<CgroupNode>,
    tasks: &[Arc<ProcessControlBlock>],
    prepared: PreparedCpuControl,
) {
    if prepared.enabled {
        for (child, group) in &prepared.groups {
            let group = group.as_ref().expect("enabled CPU css was prepared");
            group.publish_parent();
            child.set_cpu_group(Some(group.clone()));
        }
    } else {
        // The prepared vector pins every old css through detach and retire.
        for (child, _) in &prepared.groups {
            child.set_cpu_group(None);
        }
    }
    // No membership/accounting/node lock may be held across pi -> rq.
    for task in tasks {
        if !task.is_exited() && node.is_ancestor_of(&task.task_cgroup_node()) {
            attach_locked(task);
        }
    }
    if !prepared.enabled {
        for (_, group) in &prepared.groups {
            if let Some(group) = group {
                group.retire();
            }
        }
    }
}

pub(crate) fn retire_node_locked(node: &Arc<CgroupNode>) {
    if let Some(group) = node.cpu_group() {
        node.set_cpu_group(None);
        group.retire();
    }
}

pub(crate) fn attach_locked(task: &Arc<ProcessControlBlock>) {
    let group = effective_group(&task.task_cgroup_node());
    fair_group::attach_cpu_group(task, group);
}

/// Fork's task is unpublished and has never accumulated execution time.
pub(crate) fn fork_locked(task: &Arc<ProcessControlBlock>) {
    task.sched_info()
        .set_cpu_group_locked(effective_group(&task.task_cgroup_node()));
}

pub(crate) fn set_weight_locked(node: &Arc<CgroupNode>, weight: u64) -> Result<(), SystemError> {
    if !(1..=10_000).contains(&weight) {
        return Err(SystemError::ERANGE);
    }
    let mut state = node.cpu_state();
    if state.idle() {
        return Err(SystemError::EINVAL);
    }
    state.set_weight(weight);
    node.cpu_group()
        .ok_or(SystemError::ENODEV)?
        .set_shares(LoadWeight::scale_load(state.shares()))?;
    node.set_cpu_state(state);
    Ok(())
}

pub(crate) fn set_nice_locked(node: &Arc<CgroupNode>, nice: i64) -> Result<(), SystemError> {
    if !(-20..=19).contains(&nice) {
        return Err(SystemError::ERANGE);
    }
    let mut state = node.cpu_state();
    if state.idle() {
        return Err(SystemError::EINVAL);
    }
    state.set_nice(nice as i32);
    node.cpu_group()
        .ok_or(SystemError::ENODEV)?
        .set_shares(LoadWeight::scale_load(state.shares()))?;
    node.set_cpu_state(state);
    Ok(())
}

pub(crate) fn set_idle_locked(node: &Arc<CgroupNode>, idle: i64) -> Result<(), SystemError> {
    if !(0..=1).contains(&idle) {
        return Err(SystemError::EINVAL);
    }
    let mut state = node.cpu_state();
    state.set_idle(idle == 1);
    node.cpu_group()
        .ok_or(SystemError::ENODEV)?
        .set_idle(idle == 1);
    node.set_cpu_state(state);
    Ok(())
}

pub(crate) fn set_bandwidth_locked(
    node: &Arc<CgroupNode>,
    state: CgroupCpuState,
) -> Result<(), SystemError> {
    state.validate_bandwidth()?;
    let (quota, period) = state.max();
    node.cpu_group()
        .ok_or(SystemError::ENODEV)?
        .configure_bandwidth(quota, period, state.burst())?;
    node.set_cpu_state(state);
    Ok(())
}
