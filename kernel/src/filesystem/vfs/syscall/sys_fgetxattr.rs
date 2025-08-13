//! System call handler for sys_fgetxattr.

use super::xattr_utils::fd_getxattr;
use crate::{
    arch::{interrupt::TrapFrame, syscall::nr::SYS_FGETXATTR},
    syscall::table::{FormattedSyscallParam, Syscall},
};
use alloc::{string::ToString, vec::Vec};
use system_error::SystemError;

pub struct SysFgetxattrHandle;

impl Syscall for SysFgetxattrHandle {
    fn num_args(&self) -> usize {
        4
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let fd = Self::fd(args);
        let name_ptr = Self::name(args);
        let buf_ptr = Self::buf(args);
        let size = Self::size(args);

        fd_getxattr(fd, name_ptr, buf_ptr, size)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("fd", Self::fd(args).to_string()),
            FormattedSyscallParam::new("name", format!("{:#x}", Self::name(args) as usize)),
            FormattedSyscallParam::new("buf", format!("{:#x}", Self::buf(args) as usize)),
            FormattedSyscallParam::new("size", Self::size(args).to_string()),
        ]
    }
}

impl SysFgetxattrHandle {
    fn fd(args: &[usize]) -> i32 {
        args[0] as i32
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

syscall_table_macros::declare_syscall!(SYS_FGETXATTR, SysFgetxattrHandle);