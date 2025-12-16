//! System call handler for removing files or directories (unlinkat).

use system_error::SystemError;

use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_UNLINKAT;
use crate::filesystem::vfs::fcntl::AtFlags;
use crate::filesystem::vfs::vcore::{do_remove_dir, do_unlink_at};
use crate::filesystem::vfs::MAX_PATHLEN;
use crate::syscall::table::{FormattedSyscallParam, Syscall};
use crate::syscall::user_access::vfs_check_and_clone_cstr;
use alloc::vec::Vec;

pub struct SysUnlinkAtHandle;

impl Syscall for SysUnlinkAtHandle {
    /// Returns the number of arguments this syscall takes.
    fn num_args(&self) -> usize {
        3
    }

    /// **删除文件夹、取消文件的链接、删除文件的系统调用**
    ///
    /// ## 参数
    ///
    /// - `dirfd`：文件夹的文件描述符.目前暂未实现
    /// - `pathname`：文件夹的路径
    /// - `flags`：标志位
    ///
    ///
    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let dirfd = Self::dirfd(args);
        let path = Self::path(args);
        let flags = Self::flags(args);

        let flags = AtFlags::from_bits(flags as i32).ok_or(SystemError::EINVAL)?;
        let path = vfs_check_and_clone_cstr(path, Some(MAX_PATHLEN))?
            .into_string()
            .map_err(|_| SystemError::EINVAL)?;

        if flags.contains(AtFlags::AT_REMOVEDIR) {
            // debug!("rmdir");
            match do_remove_dir(dirfd, &path) {
                Err(err) => {
                    return Err(err);
                }
                Ok(_) => {
                    return Ok(0);
                }
            }
        }

        match do_unlink_at(dirfd, &path) {
            Err(err) => {
                return Err(err);
            }
            Ok(_) => {
                return Ok(0);
            }
        }
    }

    /// Formats the syscall arguments for display/debugging purposes.
    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("dirfd", format!("{:#x}", Self::dirfd(args))),
            FormattedSyscallParam::new("path", format!("{:#x}", Self::path(args) as usize)),
            FormattedSyscallParam::new("flags", format!("{:#x}", Self::flags(args))),
        ]
    }
}

impl SysUnlinkAtHandle {
    /// Extracts the dirfd argument from syscall parameters.
    fn dirfd(args: &[usize]) -> i32 {
        args[0] as i32
    }
    /// Extracts the path argument from syscall parameters.
    fn path(args: &[usize]) -> *const u8 {
        args[1] as *const u8
    }
    /// Extracts the flags argument from syscall parameters.
    fn flags(args: &[usize]) -> u32 {
        args[2] as u32
    }
}

syscall_table_macros::declare_syscall!(SYS_UNLINKAT, SysUnlinkAtHandle);
