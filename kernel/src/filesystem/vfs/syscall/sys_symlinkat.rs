//! System call handler for sys_symlinkat.

use crate::{
    arch::{interrupt::TrapFrame, syscall::nr::SYS_SYMLINKAT},
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

pub struct SysSymlinkAtHandle;

impl Syscall for SysSymlinkAtHandle {
    fn num_args(&self) -> usize {
        3
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let from = Self::from(args)?;
        let to = Self::to(args)?;
        let newdfd = Self::newdfd(args);

        do_symlinkat(from.as_str(), Some(newdfd), to.as_str())
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new(
                "from",
                Self::from(args).unwrap_or_else(|_| "<invalid>".to_string()),
            ),
            FormattedSyscallParam::new("newdfd", Self::newdfd(args).to_string()),
            FormattedSyscallParam::new(
                "to",
                Self::to(args).unwrap_or_else(|_| "<invalid>".to_string()),
            ),
        ]
    }
}

impl SysSymlinkAtHandle {
    fn from(args: &[usize]) -> Result<String, SystemError> {
        let s = vfs_check_and_clone_cstr(args[0] as *const u8, Some(MAX_PATHLEN))?
            .into_string()
            .map_err(|_| SystemError::EINVAL)?;
        Ok(s.to_string())
    }

    fn newdfd(args: &[usize]) -> i32 {
        args[1] as i32
    }

    fn to(args: &[usize]) -> Result<String, SystemError> {
        let s = vfs_check_and_clone_cstr(args[2] as *const u8, Some(MAX_PATHLEN))?
            .into_string()
            .map_err(|_| SystemError::EINVAL)?;
        Ok(s.to_string())
    }
}

syscall_table_macros::declare_syscall!(SYS_SYMLINKAT, SysSymlinkAtHandle);
