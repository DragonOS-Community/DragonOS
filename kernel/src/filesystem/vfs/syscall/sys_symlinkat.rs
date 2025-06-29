//! System call handler for sys_symlinkat.

use crate::{
    arch::{interrupt::TrapFrame, syscall::nr::SYS_SYMLINKAT},
    filesystem::vfs::MAX_PATHLEN,
    syscall::{
        table::{FormattedSyscallParam, Syscall},
        user_access::check_and_clone_cstr,
    },
};
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use system_error::SystemError;

use super::symlink_utils::do_symlinkat;

pub struct SysSymlinkAtHandle;

impl Syscall for SysSymlinkAtHandle {
    fn num_args(&self) -> usize {
        3
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let from = Self::from(args);
        let to = Self::to(args);
        let newdfd = Self::newdfd(args);

        do_symlinkat(from.as_str(), Some(newdfd), to.as_str())
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("from", Self::from(args)),
            FormattedSyscallParam::new("newdfd", Self::newdfd(args).to_string()),
            FormattedSyscallParam::new("to", Self::to(args)),
        ]
    }
}

impl SysSymlinkAtHandle {
    fn from(args: &[usize]) -> String {
        check_and_clone_cstr(args[0] as *const u8, Some(MAX_PATHLEN))
            .unwrap()
            .into_string()
            .map_err(|_| SystemError::EINVAL)
            .unwrap()
            .trim()
            .to_string()
    }

    fn newdfd(args: &[usize]) -> i32 {
        args[1] as i32
    }

    fn to(args: &[usize]) -> String {
        check_and_clone_cstr(args[2] as *const u8, Some(MAX_PATHLEN))
            .unwrap()
            .into_string()
            .map_err(|_| SystemError::EINVAL)
            .unwrap()
            .trim()
            .to_string()
    }
}

syscall_table_macros::declare_syscall!(SYS_SYMLINKAT, SysSymlinkAtHandle);
