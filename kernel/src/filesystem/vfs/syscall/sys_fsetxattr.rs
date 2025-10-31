//! System call handler for sys_fsetxattr.

use crate::{
    arch::{interrupt::TrapFrame, syscall::nr::SYS_FSETXATTR},
    filesystem::vfs::syscall::xattr_utils::fd_setxattr,
    syscall::table::{FormattedSyscallParam, Syscall},
};
use alloc::{string::ToString, vec::Vec};
use system_error::SystemError;

pub struct SysFsetxattrHandle;

impl Syscall for SysFsetxattrHandle {
    fn num_args(&self) -> usize {
        5
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let fd = Self::fd(args);
        let name_ptr = Self::name(args);
        let value_ptr = Self::value(args);
        let size = Self::size(args);
        let flags = Self::flags(args);

        fd_setxattr(fd, name_ptr, value_ptr, size, flags)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("fd", Self::fd(args).to_string()),
            FormattedSyscallParam::new("name", format!("{:#x}", Self::name(args) as usize)),
            FormattedSyscallParam::new("value", format!("{:#x}", Self::value(args) as usize)),
            FormattedSyscallParam::new("size", Self::size(args).to_string()),
            FormattedSyscallParam::new("flags", Self::flags(args).to_string()),
        ]
    }
}

impl SysFsetxattrHandle {
    fn fd(args: &[usize]) -> i32 {
        args[0] as i32
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

syscall_table_macros::declare_syscall!(SYS_FSETXATTR, SysFsetxattrHandle);
