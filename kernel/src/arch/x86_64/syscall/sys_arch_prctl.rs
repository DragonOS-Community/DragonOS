//! `arch_prctl` syscall handler: control/query arch options such as FS/GS base.
//!
//! Dispatched as a regular syscall table entry: args are re-read from the
//! frame after the ptrace entry-stop, so tracer rewrites take effect.

use crate::syscall::Syscall as SyscallDispatcher;
use crate::{
    arch::{interrupt::TrapFrame, syscall::nr::SYS_ARCH_PRCTL},
    syscall::table::{FormattedSyscallParam, Syscall},
};
use alloc::vec::Vec;
use system_error::SystemError;

pub struct SysArchPrctlHandle;

impl Syscall for SysArchPrctlHandle {
    fn num_args(&self) -> usize {
        2
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        SyscallDispatcher::arch_prctl(args[0], args[1])
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("option", format!("{:#x}", args[0])),
            FormattedSyscallParam::new("arg2", format!("{:#x}", args[1])),
        ]
    }
}

syscall_table_macros::declare_syscall!(SYS_ARCH_PRCTL, SysArchPrctlHandle);
