//! System call handler for creating hard links (linkat).

use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_LINKAT;
use crate::filesystem::vfs::syscall::AtFlags;
use crate::filesystem::vfs::MAX_PATHLEN;
use crate::syscall::table::{FormattedSyscallParam, Syscall};
use crate::syscall::user_access::check_and_clone_cstr;
use alloc::vec::Vec;
use system_error::SystemError;

pub struct SysLinkAtHandle;

impl Syscall for SysLinkAtHandle {
    /// Returns the number of arguments this syscall takes.
    fn num_args(&self) -> usize {
        5
    }

    /// Handles the linkat syscall.
    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let oldfd = Self::oldfd(args);
        let old = Self::old_path(args);
        let newfd = Self::newfd(args);
        let new = Self::new_path(args);
        let flags = Self::flags(args);

        let old = check_and_clone_cstr(old, Some(MAX_PATHLEN))?
            .into_string()
            .map_err(|_| SystemError::EINVAL)?;
        let new = check_and_clone_cstr(new, Some(MAX_PATHLEN))?
            .into_string()
            .map_err(|_| SystemError::EINVAL)?;
        if old.len() >= MAX_PATHLEN || new.len() >= MAX_PATHLEN {
            return Err(SystemError::ENAMETOOLONG);
        }
        if new.is_empty() {
            return Err(SystemError::ENOENT);
        }
        let flags = AtFlags::from_bits(flags).ok_or(SystemError::EINVAL)?;
        super::link_utils::do_linkat(oldfd, &old, newfd, &new, flags)
    }

    /// Formats the syscall arguments for display/debugging purposes.
    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("oldfd", format!("{:#x}", Self::oldfd(args))),
            FormattedSyscallParam::new("old", format!("{:#x}", Self::old_path(args) as usize)),
            FormattedSyscallParam::new("newfd", format!("{:#x}", Self::newfd(args))),
            FormattedSyscallParam::new("new", format!("{:#x}", Self::new_path(args) as usize)),
            FormattedSyscallParam::new("flags", format!("{:#x}", Self::flags(args))),
        ]
    }
}

impl SysLinkAtHandle {
    /// Extracts the oldfd argument from syscall parameters.
    fn oldfd(args: &[usize]) -> i32 {
        args[0] as i32
    }
    /// Extracts the old path argument from syscall parameters.
    fn old_path(args: &[usize]) -> *const u8 {
        args[1] as *const u8
    }
    /// Extracts the newfd argument from syscall parameters.
    fn newfd(args: &[usize]) -> i32 {
        args[2] as i32
    }
    /// Extracts the new path argument from syscall parameters.
    fn new_path(args: &[usize]) -> *const u8 {
        args[3] as *const u8
    }
    /// Extracts the flags argument from syscall parameters.
    fn flags(args: &[usize]) -> i32 {
        args[4] as i32
    }
}

syscall_table_macros::declare_syscall!(SYS_LINKAT, SysLinkAtHandle);
