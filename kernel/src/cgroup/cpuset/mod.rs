//! cgroup v2 cpuset control plane. Scheduler placement consumes only the
//! synthesized task mask; it never walks this hierarchy on the scheduling path.
mod mask;

use crate::{
    cgroup::{cgroup_root, CgroupNode},
    libs::{cpumask::CpuMask, mutex::MutexGuard},
    process::{kthread::KernelThreadFlags, ProcessControlBlock, ProcessFlags, ProcessManager},
    smp::cpu::{smp_cpu_manager, ProcessorId},
};
use alloc::{collections::BTreeSet, string::String, sync::Arc, vec::Vec};
use mask::{list, parse, IndexSet};
use system_error::SystemError;

/// Lock before hierarchy/accounting/pi/rq locks, never from interrupt context.
pub(crate) fn lock() -> MutexGuard<'static, ()> {
    super::lock()
}

#[derive(Debug, Default, Clone)]
pub(crate) struct CpusetState {
    pub(crate) generation: u64,
    cpus: IndexSet,
    mems: IndexSet,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CpusetFile {
    Cpus,
    Mems,
    EffectiveCpus,
    EffectiveMems,
}

fn cpu_indices(mask: &CpuMask) -> IndexSet {
    mask.iter_cpu().map(|c| c.data() as usize).collect()
}

fn cpu_mask(set: &IndexSet) -> CpuMask {
    let mut mask = CpuMask::new();
    for &i in set {
        mask.set(ProcessorId::new(i as u32), true);
    }
    mask
}

/// The current physical allocator has a single, flat memory domain, node 0.
/// NUMA support must replace this topology source and integrate node allocation.
fn memory_nodes() -> IndexSet {
    BTreeSet::from([0])
}

pub(crate) fn enabled(node: &Arc<CgroupNode>) -> bool {
    node.parent()
        .is_none_or(|p| p.subtree_control().iter().any(|c| c == "cpuset"))
}

fn effective(node: &Arc<CgroupNode>) -> (IndexSet, IndexSet) {
    let mut path = Vec::new();
    let mut current = Some(node.clone());
    while let Some(n) = current {
        current = n.parent();
        path.push(n);
    }
    let mut cpus = cpu_indices(&smp_cpu_manager().online_cpus());
    let mut mems = memory_nodes();
    for node in path.iter().rev().skip(1) {
        if !enabled(node) {
            continue;
        }
        let state = node.cpuset_state();
        let child_cpus: IndexSet = cpus.intersection(&state.cpus).copied().collect();
        let child_mems: IndexSet = mems.intersection(&state.mems).copied().collect();
        if !child_cpus.is_empty() {
            cpus = child_cpus;
        }
        if !child_mems.is_empty() {
            mems = child_mems;
        }
    }
    (cpus, mems)
}

pub(crate) fn allowed_locked(node: &Arc<CgroupNode>) -> CpuMask {
    cpu_mask(&effective(node).0)
}

pub(crate) fn generation(node: &Arc<CgroupNode>) -> u64 {
    node.cpuset_state().generation
}

fn check_file(node: &Arc<CgroupNode>, gen: u64) -> Result<(), SystemError> {
    if !cgroup_root().is_online(node) || !enabled(node) || generation(node) != gen {
        return Err(SystemError::ENODEV);
    }
    Ok(())
}

pub(crate) fn read(
    node: &Arc<CgroupNode>,
    gen: u64,
    file: CpusetFile,
) -> Result<Vec<u8>, SystemError> {
    let _guard = lock();
    check_file(node, gen)?;
    let state = node.cpuset_state();
    let value = match file {
        CpusetFile::Cpus => list(&state.cpus),
        CpusetFile::Mems => list(&state.mems),
        CpusetFile::EffectiveCpus => list(&effective(node).0),
        CpusetFile::EffectiveMems => list(&effective(node).1),
    };
    Ok(format!("{}\n", value).into_bytes())
}

pub(crate) fn per_cpu_task(task: &Arc<ProcessControlBlock>) -> bool {
    task.flags().contains(ProcessFlags::KTHREAD)
        && task
            .worker_private()
            .as_ref()
            .and_then(|p| p.kernel_thread())
            .is_some_and(|k| k.flags().contains(KernelThreadFlags::IS_PER_CPU))
}

fn reapply(task: &Arc<ProcessControlBlock>, node: &Arc<CgroupNode>) {
    if per_cpu_task(task) || task.is_exited() {
        return;
    }
    let allowed = allowed_locked(node);
    let request = task
        .sched_info()
        .pi_lock_irqsave()
        .user_cpus_allowed
        .clone();
    let mask = request
        .map(|r| {
            let intersection = &r & &allowed;
            if intersection.is_empty() {
                allowed.clone()
            } else {
                intersection
            }
        })
        .unwrap_or(allowed);
    ProcessManager::set_cpus_allowed(task, mask)
        .expect("a nonempty cpuset placement has no fallible migration admission");
    crate::sched::wait_cpu_placement(task);
}

/// Caller holds UPDATE_LOCK, but no accounting lock while changing placement.
pub(crate) fn attach_locked(task: &Arc<ProcessControlBlock>) {
    reapply(task, &task.task_cgroup_node());
}

fn refresh(root: &Arc<CgroupNode>, tasks: Vec<Arc<ProcessControlBlock>>) {
    for task in tasks {
        let node = task.task_cgroup_node();
        if root.is_ancestor_of(&node) {
            reapply(&task, &node);
        }
    }
}

/// Caller holds UPDATE_LOCK through admission and placement updates.
pub(crate) fn write_locked(
    node: &Arc<CgroupNode>,
    gen: u64,
    file: CpusetFile,
    input: &str,
) -> Result<(), SystemError> {
    check_file(node, gen)?;
    if node.parent().is_none() {
        return Err(SystemError::EACCES);
    }
    let valid = match file {
        CpusetFile::Cpus => cpu_indices(smp_cpu_manager().possible_cpus()),
        CpusetFile::Mems => memory_nodes(),
        _ => return Err(SystemError::EPERM),
    };
    let bits = valid.last().map_or(1, |last| last + 1);
    let values = parse(input, bits)?;
    if !values.is_subset(&valid) {
        return Err(SystemError::EINVAL);
    }
    let mut state = node.cpuset_state();
    let configured = if file == CpusetFile::Cpus {
        &mut state.cpus
    } else {
        &mut state.mems
    };
    if !configured.is_empty() && values.is_empty() && node.pids_current_count() != 0 {
        return Err(SystemError::ENOSPC);
    }
    if *configured == values {
        return Ok(());
    }
    let tasks = crate::process::snapshot_all_processes()?;
    *configured = values;
    node.set_cpuset_state(state);
    refresh(node, tasks);
    Ok(())
}

/// Called inside the subtree-control transaction after validation and snapshot.
pub(crate) fn controller_changed_locked(
    node: &Arc<CgroupNode>,
    tasks: Vec<Arc<ProcessControlBlock>>,
) {
    for child in node.children() {
        let mut state = child.cpuset_state();
        state.generation = state
            .generation
            .checked_add(1)
            .expect("cpuset generation exhausted");
        state.cpus.clear();
        state.mems.clear();
        child.set_cpuset_state(state);
    }
    refresh(node, tasks);
}

pub(crate) fn set_user_affinity(
    task: &Arc<ProcessControlBlock>,
    request: CpuMask,
) -> Result<(), SystemError> {
    let _guard = lock();
    let mask = &request & &allowed_locked(&task.task_cgroup_node());
    if mask.is_empty() {
        return Err(SystemError::EINVAL);
    }
    task.sched_info().pi_lock_irqsave().user_cpus_allowed = Some(request);
    ProcessManager::set_cpus_allowed(task, mask)?;
    crate::sched::wait_cpu_placement(task);
    Ok(())
}

pub(crate) fn status(task: &Arc<ProcessControlBlock>) -> String {
    let _guard = lock();
    let cpus = cpu_indices(&task.sched_info().cpus_allowed());
    let mems = effective(&task.task_cgroup_node()).1;
    let hex = |set: &IndexSet| -> String {
        let value = set.iter().fold(0u64, |v, &i| v | (1u64 << i));
        format!("{:08x}", value)
    };
    format!(
        "\nCpus_allowed:\t{}\nCpus_allowed_list:\t{}\nMems_allowed:\t{}\nMems_allowed_list:\t{}",
        hex(&cpus),
        list(&cpus),
        hex(&mems),
        list(&mems)
    )
}
