use alloc::{string::ToString, vec::Vec};

use system_error::SystemError;

use crate::{
    arch::{
        interrupt::TrapFrame,
        syscall::nr::{SYS_SCHED_GET_PRIORITY_MAX, SYS_SCHED_GET_PRIORITY_MIN},
    },
    syscall::table::{FormattedSyscallParam, Syscall},
};

use super::types::legacy_policy_priority_range;

struct SysSchedGetPriorityMax;

impl Syscall for SysSchedGetPriorityMax {
    fn num_args(&self) -> usize {
        1
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let policy = args[0] as i32;
        let (_, maximum) = legacy_policy_priority_range(policy).ok_or(SystemError::EINVAL)?;
        Ok(maximum as usize)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![FormattedSyscallParam::new(
            "policy",
            (args[0] as i32).to_string(),
        )]
    }
}

struct SysSchedGetPriorityMin;

impl Syscall for SysSchedGetPriorityMin {
    fn num_args(&self) -> usize {
        1
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let policy = args[0] as i32;
        let (minimum, _) = legacy_policy_priority_range(policy).ok_or(SystemError::EINVAL)?;
        Ok(minimum as usize)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![FormattedSyscallParam::new(
            "policy",
            (args[0] as i32).to_string(),
        )]
    }
}

syscall_table_macros::declare_syscall!(SYS_SCHED_GET_PRIORITY_MAX, SysSchedGetPriorityMax);
syscall_table_macros::declare_syscall!(SYS_SCHED_GET_PRIORITY_MIN, SysSchedGetPriorityMin);
