//! `rt_sigreturn` syscall handler: restore the signal context from the user stack.
//!
//! Dispatched as a regular syscall table entry, so it automatically gets the
//! ptrace entry/exit-stop, SYSEMU skip and arg-rewrite semantics
//! without any special-casing.

use crate::{
    arch::{interrupt::TrapFrame, ipc::signal::X86_64SignalArch, syscall::nr::SYS_RT_SIGRETURN},
    ipc::signal_types::SignalArch,
    syscall::table::{FormattedSyscallParam, Syscall},
};
use alloc::vec::Vec;
use system_error::SystemError;

pub struct SysRtSigreturnHandle;

impl Syscall for SysRtSigreturnHandle {
    fn num_args(&self) -> usize {
        0
    }

    fn handle(&self, _args: &[usize], frame: &mut TrapFrame) -> Result<usize, SystemError> {
        // Return the restored rax — the common tail's set_return_value writes the same value
        let r = <X86_64SignalArch as SignalArch>::sys_rt_sigreturn(frame) as usize;
        Ok(r)
    }

    fn entry_format(&self, _args: &[usize]) -> Vec<FormattedSyscallParam> {
        Vec::new()
    }
}

syscall_table_macros::declare_syscall!(SYS_RT_SIGRETURN, SysRtSigreturnHandle);
