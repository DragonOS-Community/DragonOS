//! System call handler for sys_lremovexattr.

use super::xattr_utils::path_removexattr;
use crate::{
    arch::{interrupt::TrapFrame, syscall::nr::SYS_LREMOVEXATTR},
    syscall::table::{FormattedSyscallParam, Syscall},
};
use alloc::vec::Vec;
use system_error::SystemError;

pub struct SysLremovexattrHandle;

impl Syscall for SysLremovexattrHandle {
    fn num_args(&self) -> usize {
        2
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        path_removexattr(Self::path(args), Self::name(args), 0)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("path", format!("{:#x}", Self::path(args) as usize)),
            FormattedSyscallParam::new("name", format!("{:#x}", Self::name(args) as usize)),
        ]
    }
}

impl SysLremovexattrHandle {
    fn path(args: &[usize]) -> *const u8 {
        args[0] as *const u8
    }

    fn name(args: &[usize]) -> *const u8 {
        args[1] as *const u8
    }
}

syscall_table_macros::declare_syscall!(SYS_LREMOVEXATTR, SysLremovexattrHandle);
