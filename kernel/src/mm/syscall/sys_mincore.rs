use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_MINCORE;
use crate::syscall::table::{FormattedSyscallParam, Syscall};
use system_error::SystemError;

use alloc::vec::Vec;

pub struct SysMincoreHandle;

impl Syscall for SysMincoreHandle {
    fn num_args(&self) -> usize {
        3
    }

    /// ## mincore系统调用
    ///
    /// todo: 参考 https://code.dragonos.org.cn/xref/linux-6.6.21/mm/mincore.c#232 实现mincore
    fn handle(&self, _args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        return Err(SystemError::ENOSYS);
    }

    /// Formats the syscall arguments for display/debugging purposes.
    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("start_vaddr", format!("{:#x}", Self::start_vaddr(args))),
            FormattedSyscallParam::new("len", format!("{:#x}", Self::len(args))),
            FormattedSyscallParam::new("vec", format!("{:#x}", Self::vec(args))),
        ]
    }
}

impl SysMincoreHandle {
    /// Extracts the start_vaddr argument from syscall parameters.
    fn start_vaddr(args: &[usize]) -> usize {
        args[0]
    }
    /// Extracts the len argument from syscall parameters.
    fn len(args: &[usize]) -> usize {
        args[1]
    }
    /// Extracts the
    fn vec(args: &[usize]) -> usize {
        args[2]
    }
}

syscall_table_macros::declare_syscall!(SYS_MINCORE, SysMincoreHandle);
