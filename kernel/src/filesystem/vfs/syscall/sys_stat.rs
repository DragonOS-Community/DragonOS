//! System call handler for stat.

use system_error::SystemError;

use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_STAT;
use crate::filesystem::vfs::fcntl::AtFlags;
use crate::filesystem::vfs::stat::do_newfstatat;
use crate::filesystem::vfs::MAX_PATHLEN;
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;
use crate::syscall::user_access::vfs_check_and_clone_cstr;

use alloc::vec::Vec;

pub struct SysStatHandle;

impl Syscall for SysStatHandle {
    /// Returns the number of arguments this syscall takes.
    fn num_args(&self) -> usize {
        2
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let path_ptr = Self::path(args);
        let usr_kstat = Self::usr_kstat(args);

        if usr_kstat == 0 {
            return Err(SystemError::EFAULT);
        }

        let path = vfs_check_and_clone_cstr(path_ptr, Some(MAX_PATHLEN))?;
        let path_str = path.to_str().map_err(|_| SystemError::EINVAL)?;

        // stat(2) 等价于 newfstatat(AT_FDCWD, path, buf, 0)
        // 与 lstat 不同，stat 会跟随符号链接，所以 flags = 0
        do_newfstatat(AtFlags::AT_FDCWD.bits(), path_str, usr_kstat, 0)?;

        Ok(0)
    }

    /// Formats the syscall arguments for display/debugging purposes.
    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("path", format!("{:#x}", Self::path(args) as usize)),
            FormattedSyscallParam::new("statbuf", format!("{:#x}", Self::usr_kstat(args))),
        ]
    }
}

impl SysStatHandle {
    /// Extracts the path argument from syscall parameters.
    fn path(args: &[usize]) -> *const u8 {
        args[0] as *const u8
    }

    /// Extracts the usr_kstat argument from syscall parameters.
    fn usr_kstat(args: &[usize]) -> usize {
        args[1]
    }
}

syscall_table_macros::declare_syscall!(SYS_STAT, SysStatHandle);
