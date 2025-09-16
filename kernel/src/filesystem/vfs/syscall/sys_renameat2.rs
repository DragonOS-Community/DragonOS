//! System call handler for renaming files or directories (renameat2).

use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_RENAMEAT2;
use crate::syscall::table::{FormattedSyscallParam, Syscall};
use alloc::vec::Vec;
use system_error::SystemError;

pub struct SysRenameAt2Handle;

impl Syscall for SysRenameAt2Handle {
    /// Returns the number of arguments this syscall takes.
    fn num_args(&self) -> usize {
        5
    }

    /// Handles the renameat2 syscall.
    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let oldfd = Self::oldfd(args);
        let oldname = Self::oldname(args);
        let newfd = Self::newfd(args);
        let newname = Self::newname(args);
        let flags = Self::flags(args);
        super::rename_utils::do_renameat2(oldfd, oldname, newfd, newname, flags)
    }

    /// Formats the syscall arguments for display/debugging purposes.
    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("oldfd", format!("{:#x}", Self::oldfd(args))),
            FormattedSyscallParam::new("oldname", format!("{:#x}", Self::oldname(args) as usize)),
            FormattedSyscallParam::new("newfd", format!("{:#x}", Self::newfd(args))),
            FormattedSyscallParam::new("newname", format!("{:#x}", Self::newname(args) as usize)),
            FormattedSyscallParam::new("flags", format!("{:#x}", Self::flags(args))),
        ]
    }
}

impl SysRenameAt2Handle {
    /// Extracts the oldfd argument from syscall parameters.
    fn oldfd(args: &[usize]) -> i32 {
        args[0] as i32
    }
    /// Extracts the oldname argument from syscall parameters.
    fn oldname(args: &[usize]) -> *const u8 {
        args[1] as *const u8
    }
    /// Extracts the newfd argument from syscall parameters.
    fn newfd(args: &[usize]) -> i32 {
        args[2] as i32
    }
    /// Extracts the newname argument from syscall parameters.
    fn newname(args: &[usize]) -> *const u8 {
        args[3] as *const u8
    }
    /// Extracts the flags argument from syscall parameters.
    fn flags(args: &[usize]) -> u32 {
        args[4] as u32
    }
}

syscall_table_macros::declare_syscall!(SYS_RENAMEAT2, SysRenameAt2Handle);
