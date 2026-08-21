//! `arch_prctl` 系统调用处理：控制/查询 FS/GS base 等架构相关选项。
//!
//! 作为普通 syscall 表项走统一分发路径：参数在 ptrace entry-stop
//! 之后从 frame 重取，tracer 的改写得以生效（与 Linux 一致）。

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
