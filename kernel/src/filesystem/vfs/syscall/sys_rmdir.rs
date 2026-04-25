//! System call handler for removing directories (rmdir).

use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_RMDIR;
use crate::filesystem::vfs::syscall::AtFlags;
use crate::filesystem::vfs::vcore::do_remove_dir;
use crate::filesystem::vfs::MAX_PATHLEN;
use crate::syscall::table::{FormattedSyscallParam, Syscall};
use crate::syscall::user_access::vfs_check_and_clone_cstr;
use alloc::vec::Vec;
use system_error::SystemError;

pub struct SysRmdirHandle;

impl Syscall for SysRmdirHandle {
    /// Returns the number of arguments this syscall takes.
    fn num_args(&self) -> usize {
        1
    }

    /// Handles the rmdir syscall.
    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let path = Self::path(args);
        let path = vfs_check_and_clone_cstr(path, Some(MAX_PATHLEN))?
            .into_string()
            .map_err(|_| SystemError::EINVAL)?;
        return do_remove_dir(AtFlags::AT_FDCWD.bits(), &path).map(|v| v as usize);
    }

    /// Formats the syscall arguments for display/debugging purposes.
    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![FormattedSyscallParam::new(
            "path",
            format!("{:#x}", Self::path(args) as usize),
        )]
    }
}

impl SysRmdirHandle {
    /// Extracts the path argument from syscall parameters.
    fn path(args: &[usize]) -> *const u8 {
        args[0] as *const u8
    }
}

syscall_table_macros::declare_syscall!(SYS_RMDIR, SysRmdirHandle);
