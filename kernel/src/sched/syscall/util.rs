/// 调度系统调用相关的工具函数
use alloc::{sync::Arc, vec, vec::Vec};

use system_error::SystemError;

use crate::process::cred::{CAPFlags, Cred};
use crate::process::pid::{pid_membership_lock, PidType};
use crate::process::{snapshot_all_processes, ProcessControlBlock, ProcessManager, RawPid};

use super::types::{PRIO_PGRP, PRIO_PROCESS, PRIO_USER};

/// Resolve the `which`/`who` selector pair of `getpriority()`/`setpriority()`
/// into the set of tasks the call applies to.
///
/// This is the shared half of Linux `SYSCALL_DEFINE2(getpriority)` and
/// `SYSCALL_DEFINE3(setpriority)` (`kernel/sys.c`): both walk exactly the same
/// set and differ only in what they do with it, a maximum versus an
/// assignment.
///
/// - `who == 0` selects the caller (`PRIO_PROCESS`), the caller's process group
///   (`PRIO_PGRP`) or every task owned by the caller's real uid (`PRIO_USER`).
/// - A selector that resolves to nothing is `ESRCH`. There is deliberately no
///   `EINVAL` for a negative or oversized `who`: Linux treats such a value as
///   an id that simply does not exist, and callers probe for liveness with
///   `ESRCH`.
/// - An out-of-range `which` is `EINVAL`, and Linux tests it before anything
///   else does, so it outranks every other failure.
///
/// The result is a snapshot. Both callers take per-task scheduler locks, and
/// neither the global registry lock nor a PID list lock may be held across
/// those.
pub(super) fn resolve_prio_targets(
    which: i32,
    who: i32,
) -> Result<Vec<Arc<ProcessControlBlock>>, SystemError> {
    match which {
        PRIO_PROCESS => {
            if who == 0 {
                return Ok(vec![ProcessManager::current_pcb()]);
            }
            // `find_task_by_vpid()` rather than `find_sched_target()`: the
            // latter rejects a negative pid with `EINVAL`, which is the wrong
            // answer here.
            ProcessManager::find_task_by_vpid(RawPid::from(who as usize))
                .map(|pcb| vec![pcb])
                .ok_or(SystemError::ESRCH)
        }
        PRIO_PGRP => {
            let pgrp = if who == 0 {
                ProcessManager::current_pcb().task_pgrp()
            } else {
                ProcessManager::find_vpid(RawPid::from(who as usize))
            };
            let Some(pgrp) = pgrp else {
                return Err(SystemError::ESRCH);
            };

            // The process group index holds one link per thread group, but
            // Linux's `do_each_pid_thread()` then walks every thread of each
            // linked group, and a nice value belongs to a task. Expanding here
            // keeps a thread that diverged through `PRIO_PROCESS` visible both
            // to the maximum `getpriority()` takes and to the assignment
            // `setpriority()` performs.
            //
            // The membership lock is held for the same reason `kill_pgrp`
            // takes it: `setpgid()`, `setsid()` and `de_thread()` move these
            // links, and a group observed mid-transfer has no completed set of
            // members.
            let leaders: Vec<_> = {
                let _membership_guard = pid_membership_lock();
                pgrp.tasks_iter(PidType::PGID).collect()
            };
            let mut tasks = Vec::new();
            for leader in leaders {
                tasks.extend(ProcessManager::thread_group_tasks_snapshot(leader));
            }
            if tasks.is_empty() {
                return Err(SystemError::ESRCH);
            }
            Ok(tasks)
        }
        PRIO_USER => {
            // DragonOS has a single uid space, so comparing against `Cred::uid`
            // directly is the whole of Linux's `make_kuid()` plus `uid_eq()`
            // here. A `who` that names no uid matches no task, which Linux
            // reports as `ESRCH` as well.
            let uid = if who == 0 {
                ProcessManager::current_pcb().cred().uid.data()
            } else {
                who as usize
            };
            // Linux filters on `task_pid_vnr() != 0` so that the swapper task
            // and anything living outside the caller's PID namespace drops out
            // of the `for_each_process_thread()` walk.
            let tasks: Vec<_> = snapshot_all_processes()?
                .into_iter()
                .filter(|pcb| pcb.task_pid_vnr().data() != 0 && pcb.cred().uid.data() == uid)
                .collect();
            if tasks.is_empty() {
                return Err(SystemError::ESRCH);
            }
            Ok(tasks)
        }
        _ => Err(SystemError::EINVAL),
    }
}

/// Resolve a legacy scheduler syscall PID as a thread ID in the caller's
/// active PID namespace.
pub(super) fn find_sched_target(pid: i32) -> Result<Arc<ProcessControlBlock>, SystemError> {
    if pid < 0 {
        return Err(SystemError::EINVAL);
    }

    if pid == 0 {
        Ok(ProcessManager::current_pcb())
    } else {
        ProcessManager::find_task_by_vpid(RawPid::from(pid as usize)).ok_or(SystemError::ESRCH)
    }
}

/// Linux sched_setscheduler owner rule: the caller's effective UID must match
/// either the target's real or effective UID.
#[inline]
pub(super) fn same_sched_owner(current: &Cred, target: &Cred) -> bool {
    current.euid == target.euid || current.euid == target.uid
}

/// 检查当前进程是否有权限查询目标进程的调度信息
///
/// 权限规则（与 Linux 一致）：
/// - 进程自己可以查询
/// - 具有 CAP_SYS_NICE 权限的进程可以查询
/// - root 用户（uid == 0）可以查询
///
/// # Arguments
/// * `current_pcb` - 当前进程的 PCB
/// * `target_pcb` - 目标进程的 PCB
///
/// # Returns
/// * `true` - 有权限
/// * `false` - 无权限
pub fn has_sched_permission(
    current_pcb: &ProcessControlBlock,
    target_pcb: &ProcessControlBlock,
) -> bool {
    // 进程自己
    if current_pcb.raw_pid() == target_pcb.raw_pid() {
        return true;
    }

    let current_cred = current_pcb.cred();

    // 具有 CAP_SYS_NICE 权限
    if current_cred.has_capability(CAPFlags::CAP_SYS_NICE) {
        return true;
    }

    // root 用户（uid == 0）
    current_cred.uid.data() == 0
}

/// 检查当前进程是否有权限修改目标进程的 CPU affinity。
///
/// Linux 兼容语义：
/// - 进程自己总是允许
/// - 具有 CAP_SYS_NICE 的进程允许
/// - root（euid == 0）允许
/// - 同一用户（real/effective uid 匹配）允许
pub fn has_sched_setaffinity_permission(
    current_pcb: &ProcessControlBlock,
    target_pcb: &ProcessControlBlock,
) -> bool {
    if current_pcb.raw_pid() == target_pcb.raw_pid() {
        return true;
    }

    let current_cred = current_pcb.cred();
    if current_cred.has_capability(CAPFlags::CAP_SYS_NICE) {
        return true;
    }

    if current_cred.euid.data() == 0 {
        return true;
    }

    let target_cred = target_pcb.cred();
    current_cred.euid == target_cred.euid
        || current_cred.euid == target_cred.uid
        || current_cred.uid == target_cred.euid
        || current_cred.uid == target_cred.uid
}
