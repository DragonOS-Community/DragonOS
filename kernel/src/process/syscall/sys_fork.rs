use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_FORK;
use crate::process::ProcessManager;
use crate::process::fork::CloneFlags;
use crate::syscall::table::{FormattedSyscallParam, Syscall};
use alloc::vec::Vec;
use system_error::SystemError;

pub struct SysFork;

impl Syscall for SysFork {
    fn num_args(&self) -> usize {
        0
    }

    fn handle(&self, _args: &[usize], frame: &mut TrapFrame) -> Result<usize, SystemError> {
        ProcessManager::fork(frame, CloneFlags::empty()).map(|pid| pid.into())
    }

    fn entry_format(&self, _args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![]
    }
}

syscall_table_macros::declare_syscall!(SYS_FORK, SysFork);
