//! Common cgroup.procs/cgroup.threads migration transaction.
use alloc::{sync::Arc, vec::Vec};
use system_error::SystemError;

use super::CgroupOpenState;
use crate::{
    cgroup::{cgroup_accounting_lock, cgroup_migrate_vet_dst, cgroup_root, cpuset, CgroupNode},
    process::{ProcessFlags, ProcessManager, RawPid},
};

/// Linux kstrtoint(base=0), restricted to nonnegative PID values.
fn parse_pid(buf: &[u8]) -> Result<usize, SystemError> {
    let input = core::str::from_utf8(buf)
        .map_err(|_| SystemError::EINVAL)?
        .trim();
    let negative = input.starts_with('-');
    let input = input
        .strip_prefix('+')
        .or_else(|| input.strip_prefix('-'))
        .unwrap_or(input);
    let (digits, radix) = if let Some(hex) = input
        .strip_prefix("0x")
        .or_else(|| input.strip_prefix("0X"))
    {
        (hex, 16)
    } else if input.len() > 1 && input.starts_with('0') {
        (&input[1..], 8)
    } else {
        (input, 10)
    };
    if digits.starts_with('+') || digits.starts_with('-') {
        return Err(SystemError::EINVAL);
    }
    let pid = u32::from_str_radix(digits, radix).map_err(|_| SystemError::EINVAL)?;
    if pid > i32::MAX as u32 || (negative && pid != 0) {
        return Err(SystemError::EINVAL);
    }
    Ok(pid as usize)
}

/// Caller holds the update lock and has validated target file liveness.
pub(super) fn write_locked(
    dst: &Arc<CgroupNode>,
    buf: &[u8],
    group: bool,
    open: &CgroupOpenState,
) -> Result<usize, SystemError> {
    let pid = parse_pid(buf)?;
    let current = ProcessManager::current_pcb();
    let selected = if pid == 0 {
        current
    } else {
        ProcessManager::find_task_by_vpid(RawPid::new(pid)).ok_or(SystemError::ESRCH)?
    };
    let anchor = if group {
        selected
            .threads_read_irqsave()
            .group_leader()
            .unwrap_or_else(|| selected.clone())
    } else {
        selected
    };
    let src = anchor.task_cgroup_node();
    if !cgroup_root().is_online(&src) {
        return Err(SystemError::ESRCH);
    }
    if cgroup_root().nsdelegate() {
        let root = open.namespace.root_cgroup();
        if !root.is_ancestor_of(&src) || !root.is_ancestor_of(dst) {
            return Err(SystemError::ENOENT);
        }
    }
    // The target file's write permission was checked at open. Both interfaces
    // additionally require the opener to write the common ancestor's procs.
    super::permissions::check_common_ancestor(&src, dst, &open.cred)?;
    let source_domain = src.resource_domain();
    if !group && !Arc::ptr_eq(&source_domain, &dst.resource_domain()) {
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }
    let mut tasks = Vec::new();
    tasks.push(anchor.clone());
    if group {
        for weak in anchor.threads_read_irqsave().group_tasks_clone() {
            if let Some(task) = weak.upgrade() {
                if !Arc::ptr_eq(&task, &anchor) {
                    tasks.push(task);
                }
            }
        }
    }
    {
        let _membership = crate::process::pid::pid_membership_lock();
        let _accounting = cgroup_accounting_lock().lock();
        tasks.retain(|task| !task.is_exited() && !task.flags().contains(ProcessFlags::EXITING));
        if tasks.is_empty() {
            return Err(SystemError::ESRCH);
        }
        cgroup_migrate_vet_dst(dst)?;
        for task in &tasks {
            if cpuset::per_cpu_task(task) {
                return Err(SystemError::EINVAL);
            }
            if !Arc::ptr_eq(&task.task_cgroup_node().resource_domain(), &source_domain) {
                return Err(SystemError::EINVAL);
            }
        }
        // pids.max restricts creation, not organizational migration.
        for task in &tasks {
            task.set_task_cgroup_node(dst.clone());
        }
    }
    for task in &tasks {
        cpuset::attach_locked(task);
    }
    Ok(buf.len())
}
