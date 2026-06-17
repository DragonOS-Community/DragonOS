//! System call handler for sys_listxattr.

use super::xattr_utils::path_listxattr;
use crate::{
    arch::{interrupt::TrapFrame, syscall::nr::SYS_LISTXATTR},
    filesystem::vfs::VFS_MAX_FOLLOW_SYMLINK_TIMES,
    syscall::table::{FormattedSyscallParam, Syscall},
};
use alloc::{string::ToString, vec::Vec};
use system_error::SystemError;

pub struct SysListxattrHandle;

impl Syscall for SysListxattrHandle {
    fn num_args(&self) -> usize {
        3
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        path_listxattr(
            Self::path(args),
            Self::buf(args),
            Self::size(args),
            VFS_MAX_FOLLOW_SYMLINK_TIMES,
        )
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("path", format!("{:#x}", Self::path(args) as usize)),
            FormattedSyscallParam::new("buf", format!("{:#x}", Self::buf(args) as usize)),
            FormattedSyscallParam::new("size", Self::size(args).to_string()),
        ]
    }
}

impl SysListxattrHandle {
    fn path(args: &[usize]) -> *const u8 {
        args[0] as *const u8
    }

    fn buf(args: &[usize]) -> *mut u8 {
        args[1] as *mut u8
    }

    fn size(args: &[usize]) -> usize {
        args[2]
    }
}

syscall_table_macros::declare_syscall!(SYS_LISTXATTR, SysListxattrHandle);
