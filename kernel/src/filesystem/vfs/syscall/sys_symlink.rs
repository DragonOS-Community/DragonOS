//! System call handler for sys_symlink.

use crate::{
    arch::{interrupt::TrapFrame, syscall::nr::SYS_SYMLINK},
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

pub struct SysSymlinkHandle;

impl Syscall for SysSymlinkHandle {
    fn num_args(&self) -> usize {
        2
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let from = Self::from(args);
        let to = Self::to(args);

        do_symlinkat(from.as_str(), None, to.as_str())
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("from", Self::from(args)),
            FormattedSyscallParam::new("to", Self::to(args)),
        ]
    }
}

impl SysSymlinkHandle {
    fn from(args: &[usize]) -> String {
        check_and_clone_cstr(args[0] as *const u8, Some(MAX_PATHLEN))
            .unwrap()
            .into_string()
            .map_err(|_| SystemError::EINVAL)
            .unwrap()
            .trim()
            .to_string()
    }

    fn to(args: &[usize]) -> String {
        check_and_clone_cstr(args[1] as *const u8, Some(MAX_PATHLEN))
            .unwrap()
            .into_string()
            .map_err(|_| SystemError::EINVAL)
            .unwrap()
            .trim()
            .to_string()
    }
}

syscall_table_macros::declare_syscall!(SYS_SYMLINK, SysSymlinkHandle);
