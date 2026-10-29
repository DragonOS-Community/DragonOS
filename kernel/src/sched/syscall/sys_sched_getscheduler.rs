use alloc::{string::ToString, vec::Vec};

use system_error::SystemError;

use crate::{
    arch::{interrupt::TrapFrame, syscall::nr::SYS_SCHED_GETSCHEDULER},
    syscall::table::{FormattedSyscallParam, Syscall},
};

use super::{
    types::{policy_to_linux, SCHED_RESET_ON_FORK},
    util::find_sched_target,
};

struct SysSchedGetscheduler;

impl Syscall for SysSchedGetscheduler {
    fn num_args(&self) -> usize {
        1
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let pid = args[0] as i32;
        let target = find_sched_target(pid)?;
        let (policy, reset_on_fork) = {
            let pi_guard = target.sched_info().pi_lock_irqsave();
            (target.sched_info().policy(), pi_guard.sched_reset_on_fork())
        };

        let mut linux_policy = policy_to_linux(policy);
        if reset_on_fork {
            linux_policy |= SCHED_RESET_ON_FORK;
        }
        Ok(linux_policy as usize)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![FormattedSyscallParam::new(
            "pid",
            (args[0] as i32).to_string(),
        )]
    }
}

syscall_table_macros::declare_syscall!(SYS_SCHED_GETSCHEDULER, SysSchedGetscheduler);
