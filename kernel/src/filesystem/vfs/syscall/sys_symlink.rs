//! System call handler for sys_symlink.

use crate::{
    arch::{interrupt::TrapFrame, syscall::nr::SYS_SYMLINK},
    filesystem::vfs::MAX_PATHLEN,
    syscall::{
        table::{FormattedSyscallParam, Syscall},
        user_access::vfs_check_and_clone_cstr,
    },
};
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use system_error::SystemError;

use super::symlink_utils::do_symlinkat;

pub struct SysSymlinkHandle;

impl Syscall for SysSymlinkHandle {
    fn num_args(&self) -> usize {
        2
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let from = Self::from(args)?;
        let to = Self::to(args)?;

        do_symlinkat(from.as_str(), None, to.as_str())
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new(
                "from",
                Self::from(args).unwrap_or_else(|_| "<invalid>".to_string()),
            ),
            FormattedSyscallParam::new(
                "to",
                Self::to(args).unwrap_or_else(|_| "<invalid>".to_string()),
            ),
        ]
    }
}

impl SysSymlinkHandle {
    fn from(args: &[usize]) -> Result<String, SystemError> {
        let s = vfs_check_and_clone_cstr(args[0] as *const u8, Some(MAX_PATHLEN))?
            .into_string()
            .map_err(|_| SystemError::EINVAL)?;
        Ok(s.to_string())
    }

    fn to(args: &[usize]) -> Result<String, SystemError> {
        let s = vfs_check_and_clone_cstr(args[1] as *const u8, Some(MAX_PATHLEN))?
            .into_string()
            .map_err(|_| SystemError::EINVAL)?;
        Ok(s.to_string())
    }
}

syscall_table_macros::declare_syscall!(SYS_SYMLINK, SysSymlinkHandle);
