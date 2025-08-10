//! System call handler for creating hard links (link).

use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_LINK;
use crate::filesystem::vfs::MAX_PATHLEN;
use crate::filesystem::vfs::syscall::AtFlags;
use crate::syscall::table::{FormattedSyscallParam, Syscall};
use crate::syscall::user_access::check_and_clone_cstr;
use alloc::string::String;
use alloc::vec::Vec;
use system_error::SystemError;

pub struct SysLinkHandle;

impl Syscall for SysLinkHandle {
    /// Returns the number of arguments this syscall takes.
    fn num_args(&self) -> usize {
        2
    }

    /// Handles the link syscall.
    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let old = Self::old_path(args);
        let new = Self::new_path(args);

        let get_path = |cstr: *const u8| -> Result<String, SystemError> {
            let res = check_and_clone_cstr(cstr, Some(MAX_PATHLEN))?
                .into_string()
                .map_err(|_| SystemError::EINVAL)?;
            if res.len() >= MAX_PATHLEN {
                return Err(SystemError::ENAMETOOLONG);
            }
            if res.is_empty() {
                return Err(SystemError::ENOENT);
            }
            Ok(res)
        };

        let old = get_path(old)?;
        let new = get_path(new)?;
        super::link_utils::do_linkat(
            AtFlags::AT_FDCWD.bits(),
            &old,
            AtFlags::AT_FDCWD.bits(),
            &new,
            AtFlags::empty(),
        )
    }

    /// Formats the syscall arguments for display/debugging purposes.
    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("old", format!("{:#x}", Self::old_path(args) as usize)),
            FormattedSyscallParam::new("new", format!("{:#x}", Self::new_path(args) as usize)),
        ]
    }
}

impl SysLinkHandle {
    /// Extracts the old path argument from syscall parameters.
    fn old_path(args: &[usize]) -> *const u8 {
        args[0] as *const u8
    }
    /// Extracts the new path argument from syscall parameters.
    fn new_path(args: &[usize]) -> *const u8 {
        args[1] as *const u8
    }
}

syscall_table_macros::declare_syscall!(SYS_LINK, SysLinkHandle);
