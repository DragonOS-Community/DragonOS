//! System call handler for sys_fremovexattr.

use crate::{
    arch::{interrupt::TrapFrame, syscall::nr::SYS_FREMOVEXATTR},
    filesystem::vfs::syscall::xattr_utils::fd_removexattr,
    syscall::table::{FormattedSyscallParam, Syscall},
};
use alloc::{string::ToString, vec::Vec};
use system_error::SystemError;

pub struct SysFremovexattrHandle;

impl Syscall for SysFremovexattrHandle {
    fn num_args(&self) -> usize {
        2
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let fd = Self::fd(args);
        let name_ptr = Self::name(args);

        fd_removexattr(fd, name_ptr)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("fd", Self::fd(args).to_string()),
            FormattedSyscallParam::new("name", format!("{:#x}", Self::name(args) as usize)),
        ]
    }
}

impl SysFremovexattrHandle {
    fn fd(args: &[usize]) -> i32 {
        args[0] as i32
    }

    fn name(args: &[usize]) -> *const u8 {
        args[1] as *const u8
    }
}

syscall_table_macros::declare_syscall!(SYS_FREMOVEXATTR, SysFremovexattrHandle);
