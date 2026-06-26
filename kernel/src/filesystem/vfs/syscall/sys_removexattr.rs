//! System call handler for sys_removexattr.

use super::xattr_utils::path_removexattr;
use crate::{
    arch::{interrupt::TrapFrame, syscall::nr::SYS_REMOVEXATTR},
    filesystem::vfs::VFS_MAX_FOLLOW_SYMLINK_TIMES,
    syscall::table::{FormattedSyscallParam, Syscall},
};
use alloc::vec::Vec;
use system_error::SystemError;

pub struct SysRemovexattrHandle;

impl Syscall for SysRemovexattrHandle {
    fn num_args(&self) -> usize {
        2
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        path_removexattr(
            Self::path(args),
            Self::name(args),
            VFS_MAX_FOLLOW_SYMLINK_TIMES,
        )
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("path", format!("{:#x}", Self::path(args) as usize)),
            FormattedSyscallParam::new("name", format!("{:#x}", Self::name(args) as usize)),
        ]
    }
}

impl SysRemovexattrHandle {
    fn path(args: &[usize]) -> *const u8 {
        args[0] as *const u8
    }

    fn name(args: &[usize]) -> *const u8 {
        args[1] as *const u8
    }
}

syscall_table_macros::declare_syscall!(SYS_REMOVEXATTR, SysRemovexattrHandle);
