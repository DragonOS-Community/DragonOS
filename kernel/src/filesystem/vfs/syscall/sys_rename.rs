//! System call handler for renaming files or directories (rename).

use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_RENAME;
use crate::filesystem::vfs::syscall::AtFlags;
use crate::syscall::table::{FormattedSyscallParam, Syscall};
use alloc::vec::Vec;
use system_error::SystemError;

pub struct SysRenameHandle;

impl Syscall for SysRenameHandle {
    /// Returns the number of arguments this syscall takes.
    fn num_args(&self) -> usize {
        2
    }

    /// Handles the rename syscall.
    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let oldname = Self::oldname(args);
        let newname = Self::newname(args);
        super::rename_utils::do_renameat2(
            AtFlags::AT_FDCWD.bits(),
            oldname,
            AtFlags::AT_FDCWD.bits(),
            newname,
            0,
        )
    }

    /// Formats the syscall arguments for display/debugging purposes.
    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("oldname", format!("{:#x}", Self::oldname(args) as usize)),
            FormattedSyscallParam::new("newname", format!("{:#x}", Self::newname(args) as usize)),
        ]
    }
}

impl SysRenameHandle {
    /// Extracts the oldname argument from syscall parameters.
    fn oldname(args: &[usize]) -> *const u8 {
        args[0] as *const u8
    }
    /// Extracts the newname argument from syscall parameters.
    fn newname(args: &[usize]) -> *const u8 {
        args[1] as *const u8
    }
}

syscall_table_macros::declare_syscall!(SYS_RENAME, SysRenameHandle);
