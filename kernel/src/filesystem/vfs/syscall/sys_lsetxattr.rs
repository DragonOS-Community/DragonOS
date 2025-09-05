//! System call handler for sys_lsetxattr.

use crate::{
    arch::{interrupt::TrapFrame, syscall::nr::SYS_LSETXATTR},
    filesystem::vfs::syscall::xattr_utils::path_setxattr,
    syscall::table::{FormattedSyscallParam, Syscall},
};
use alloc::{string::ToString, vec::Vec};
use system_error::SystemError;

pub struct SysLsetxattrHandle;

impl Syscall for SysLsetxattrHandle {
    fn num_args(&self) -> usize {
        5
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let path_ptr = Self::path(args);
        let name_ptr = Self::name(args);
        let value_ptr = Self::value(args);
        let size = Self::size(args);
        let flags = Self::flags(args);

        path_setxattr(path_ptr, name_ptr, value_ptr, size, 0, flags)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("path", format!("{:#x}", Self::path(args) as usize)),
            FormattedSyscallParam::new("name", format!("{:#x}", Self::name(args) as usize)),
            FormattedSyscallParam::new("value", format!("{:#x}", Self::value(args) as usize)),
            FormattedSyscallParam::new("size", Self::size(args).to_string()),
            FormattedSyscallParam::new("flags", Self::flags(args).to_string()),
        ]
    }
}

impl SysLsetxattrHandle {
    fn path(args: &[usize]) -> *const u8 {
        args[0] as *const u8
    }

    fn name(args: &[usize]) -> *const u8 {
        args[1] as *const u8
    }

    fn value(args: &[usize]) -> *const u8 {
        args[2] as *const u8
    }

    fn size(args: &[usize]) -> usize {
        args[3]
    }

    fn flags(args: &[usize]) -> i32 {
        args[4] as i32
    }
}

syscall_table_macros::declare_syscall!(SYS_LSETXATTR, SysLsetxattrHandle);
