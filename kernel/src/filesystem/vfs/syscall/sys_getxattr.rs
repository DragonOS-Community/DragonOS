//! System call handler for sys_getxattr.

use super::xattr_utils::path_getxattr;
use crate::{
    arch::{interrupt::TrapFrame, syscall::nr::SYS_GETXATTR},
    filesystem::vfs::VFS_MAX_FOLLOW_SYMLINK_TIMES,
    syscall::table::{FormattedSyscallParam, Syscall},
};
use alloc::{string::ToString, vec::Vec};
use system_error::SystemError;

pub struct SysGetxattrHandle;

impl Syscall for SysGetxattrHandle {
    fn num_args(&self) -> usize {
        4
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let path_ptr = Self::path(args);
        let name_ptr = Self::name(args);
        let buf_ptr = Self::buf(args);
        let size = Self::size(args);        

        path_getxattr(path_ptr, name_ptr, buf_ptr, size, VFS_MAX_FOLLOW_SYMLINK_TIMES)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("path", format!("{:#x}", Self::path(args) as usize)),
            FormattedSyscallParam::new("name", format!("{:#x}", Self::name(args) as usize)),
            FormattedSyscallParam::new("buf", format!("{:#x}", Self::buf(args) as usize)),
            FormattedSyscallParam::new("size", Self::size(args).to_string()),
        ]
    }
}

impl SysGetxattrHandle {
    fn path(args: &[usize]) -> *const u8 {
        args[0] as *const u8
    }

    fn name(args: &[usize]) -> *const u8 {
        args[1] as *const u8
    }

    fn buf(args: &[usize]) -> *mut u8 {
        args[2] as *mut u8
    }

    fn size(args: &[usize]) -> usize {
        args[3]
    }
}

syscall_table_macros::declare_syscall!(SYS_GETXATTR, SysGetxattrHandle);