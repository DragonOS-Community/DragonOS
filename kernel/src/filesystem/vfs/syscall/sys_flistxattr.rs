//! System call handler for sys_flistxattr.

use super::xattr_utils::fd_listxattr;
use crate::{
    arch::{interrupt::TrapFrame, syscall::nr::SYS_FLISTXATTR},
    syscall::table::{FormattedSyscallParam, Syscall},
};
use alloc::{string::ToString, vec::Vec};
use system_error::SystemError;

pub struct SysFlistxattrHandle;

impl Syscall for SysFlistxattrHandle {
    fn num_args(&self) -> usize {
        3
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        fd_listxattr(Self::fd(args), Self::buf(args), Self::size(args))
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("fd", Self::fd(args).to_string()),
            FormattedSyscallParam::new("buf", format!("{:#x}", Self::buf(args) as usize)),
            FormattedSyscallParam::new("size", Self::size(args).to_string()),
        ]
    }
}

impl SysFlistxattrHandle {
    fn fd(args: &[usize]) -> i32 {
        args[0] as i32
    }

    fn buf(args: &[usize]) -> *mut u8 {
        args[1] as *mut u8
    }

    fn size(args: &[usize]) -> usize {
        args[2]
    }
}

syscall_table_macros::declare_syscall!(SYS_FLISTXATTR, SysFlistxattrHandle);
