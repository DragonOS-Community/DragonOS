use alloc::{string::ToString, sync::Arc, vec::Vec};

use system_error::SystemError;

use crate::{
    arch::{interrupt::TrapFrame, syscall::nr::SYS_SETPRIORITY},
    process::{
        cred::{capable, ns_capable, CAPFlags},
        resource::RLimitID,
        ProcessControlBlock, ProcessManager,
    },
    sched::prio::{PrioUtil, MAX_NICE, MIN_NICE},
    syscall::table::{FormattedSyscallParam, Syscall},
};

use super::util::{resolve_prio_targets, same_sched_owner};

struct SysSetpriority;

/// Linux threads a single `int error` through every target `setpriority()`
/// visits: it starts at `-ESRCH`, a failure overwrites it, and a success only
/// ever clears that initial `-ESRCH`. Two consequences are user visible and are
/// modelled here rather than collapsed into "any failure wins": a rejection by
/// one target survives a later target's success, and the reported error is the
/// last rejection rather than the first.
enum PrioResult {
    /// No target has been visited yet.
    NoTarget,
    /// At least one target was visited and none of them was rejected.
    Applied,
    /// The most recent rejection.
    Failed(SystemError),
}

impl PrioResult {
    fn into_syscall_result(self) -> Result<usize, SystemError> {
        match self {
            PrioResult::NoTarget => Err(SystemError::ESRCH),
            PrioResult::Applied => Ok(0),
            PrioResult::Failed(error) => Err(error),
        }
    }
}

impl SysSetpriority {
    /// Linux `set_one_prio()` (`kernel/sys.c`). DragonOS has no
    /// `security_task_setnice()` LSM hook, so that step has no counterpart.
    ///
    /// Linux decides and then commits: `can_nice()` is evaluated against the
    /// target's current nice value and `set_user_nice()` immediately applies
    /// the new one. There is no second look at the old value, because the nice
    /// value is absolute rather than a delta and so nothing in the commit
    /// depends on the value the decision was made against.
    fn set_one_prio(
        target: &Arc<ProcessControlBlock>,
        nice: i32,
        carried: PrioResult,
    ) -> PrioResult {
        if !Self::permits(target) {
            return PrioResult::Failed(SystemError::EPERM);
        }

        // The limit is only consulted when the change raises the task's
        // priority. Linux spells the test `niceval < task_nice(p)`.
        if nice < PrioUtil::prio_to_nice(target.sched_info().static_prio())
            && !Self::can_nice(target, nice)
        {
            return PrioResult::Failed(SystemError::EACCES);
        }

        if let Err(error) = ProcessManager::set_scheduler_nice(target, nice) {
            // Linux has no failure of `set_user_nice()` to charge here. The one
            // DragonOS state that can produce one is a target the scheduler
            // cannot update at all, and it is charged to that target alone so
            // that the remaining members of the set are still visited.
            return PrioResult::Failed(error);
        }

        match carried {
            PrioResult::NoTarget => PrioResult::Applied,
            carried => carried,
        }
    }

    /// Linux `set_one_prio_perm()`: the caller's effective uid has to match the
    /// target's real or effective uid, or the caller has to hold
    /// `CAP_SYS_NICE` in the *target's* user namespace.
    fn permits(target: &Arc<ProcessControlBlock>) -> bool {
        let current = ProcessManager::current_pcb();
        same_sched_owner(&current.cred(), &target.cred())
            || ns_capable(&target.cred().user_ns, CAPFlags::CAP_SYS_NICE)
    }

    /// Linux `can_nice()`: the requested nice value, re-encoded as
    /// `RLIMIT_NICE` units, has to fit under the target's soft limit unless the
    /// caller holds `CAP_SYS_NICE`.
    ///
    /// Linux compares an `int` against an `unsigned long` here, so the bound is
    /// unsigned, and that is what lets `RLIM_INFINITY` mean "no limit" instead
    /// of "reject everything".
    fn can_nice(target: &Arc<ProcessControlBlock>, nice: i32) -> bool {
        PrioUtil::nice_to_rlimit(nice) as u64 <= target.get_rlimit(RLimitID::Nice).rlim_cur
            || capable(CAPFlags::CAP_SYS_NICE)
    }
}

/// Linux `SYSCALL_DEFINE3(setpriority)` (`kernel/sys.c`).
impl Syscall for SysSetpriority {
    fn num_args(&self) -> usize {
        3
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let which = args[0] as i32;
        let who = args[1] as i32;
        let niceval = args[2] as i32;

        // Linux normalizes an out-of-range `niceval` rather than rejecting it:
        // anything below `MIN_NICE` becomes `MIN_NICE` and anything above
        // `MAX_NICE` becomes `MAX_NICE`. `EINVAL` is reserved for `which`.
        let nice = niceval.clamp(MIN_NICE, MAX_NICE);

        // `resolve_prio_targets()` is what rejects an out-of-range `which`, and
        // Linux orders that check ahead of every other failure.
        let mut result = PrioResult::NoTarget;
        for target in resolve_prio_targets(which, who)? {
            result = Self::set_one_prio(&target, nice, result);
        }
        result.into_syscall_result()
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("which", (args[0] as i32).to_string()),
            FormattedSyscallParam::new("who", (args[1] as i32).to_string()),
            FormattedSyscallParam::new("niceval", (args[2] as i32).to_string()),
        ]
    }
}

syscall_table_macros::declare_syscall!(SYS_SETPRIORITY, SysSetpriority);
