use system_error::SystemError;

use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_LSTAT;
use crate::filesystem::vfs::fcntl::AtFlags;
use crate::filesystem::vfs::stat::do_newfstatat;
use crate::filesystem::vfs::MAX_PATHLEN;
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;
use crate::syscall::user_access::vfs_check_and_clone_cstr;
use alloc::vec::Vec;

pub struct SysLstatHandle;

impl Syscall for SysLstatHandle {
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

        // lstat(2) 等价于 newfstatat(AT_FDCWD, path, buf, AT_SYMLINK_NOFOLLOW)，
        // 且 Linux 语义要求：当 path 以 '/' 结尾时必须解析为目录，此时会跟随 symlink。
        do_newfstatat(
            AtFlags::AT_FDCWD.bits(),
            path_str,
            usr_kstat,
            AtFlags::AT_SYMLINK_NOFOLLOW.bits() as u32,
        )?;
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

impl SysLstatHandle {
    /// Extracts the path argument from syscall parameters.
    fn path(args: &[usize]) -> *const u8 {
        args[0] as *const u8
    }

    /// Extracts the usr_kstat argument from syscall parameters.
    fn usr_kstat(args: &[usize]) -> usize {
        args[1]
    }
}

syscall_table_macros::declare_syscall!(SYS_LSTAT, SysLstatHandle);
