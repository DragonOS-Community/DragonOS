//! System call handler for sys_lgetxattr.

use super::xattr_utils::path_getxattr;
use crate::{
    arch::{interrupt::TrapFrame, syscall::nr::SYS_LGETXATTR},
    syscall::{user_access::UserBufferWriter, table::{FormattedSyscallParam, Syscall}},
};
use alloc::{string::ToString, vec::Vec};
use system_error::SystemError;

pub struct SysLgetxattrHandle;

impl Syscall for SysLgetxattrHandle {
    fn num_args(&self) -> usize {
        4
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let path_ptr = Self::path(args);
        let name_ptr = Self::name(args);
        let value_ptr = Self::value(args);
        let size = Self::size(args);

        let mut user_buffer_writer = UserBufferWriter::new(value_ptr, size, _frame.is_from_user())?;
        let user_buf = user_buffer_writer.buffer(0)?;

        path_getxattr(path_ptr, name_ptr, user_buf, size, 0)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("path", format!("{:#x}", Self::path(args) as usize)),
            FormattedSyscallParam::new("name", format!("{:#x}", Self::name(args) as usize)),
            FormattedSyscallParam::new("value", format!("{:#x}", Self::value(args) as usize)),
            FormattedSyscallParam::new("size", Self::size(args).to_string()),
        ]
    }
}

impl SysLgetxattrHandle {
    fn path(args: &[usize]) -> *const u8 {
        args[0] as *const u8
    }

    fn name(args: &[usize]) -> *const u8 {
        args[1] as *const u8
    }

    fn value(args: &[usize]) -> *mut u8 {
        args[2] as *mut u8
    }

    fn size(args: &[usize]) -> usize {
        args[3]
    }
}

syscall_table_macros::declare_syscall!(SYS_LGETXATTR, SysLgetxattrHandle);