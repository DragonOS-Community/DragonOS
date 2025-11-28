//! System call handler for creating directories (mkdirat).

use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_MKDIRAT;
use crate::filesystem::vfs::syscall::ModeType;
use crate::filesystem::vfs::vcore::do_mkdir_at;
use crate::syscall::table::{FormattedSyscallParam, Syscall};
use alloc::vec::Vec;
use system_error::SystemError;

pub struct SysMkdirAtHandle;

impl Syscall for SysMkdirAtHandle {
    /// Returns the number of arguments this syscall takes.
    fn num_args(&self) -> usize {
        3
    }

    /// Handles the mkdirat syscall.
    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let dirfd = Self::dirfd(args);
        let path = Self::path(args);
        let mode = Self::mode(args);

        let path = crate::filesystem::vfs::syscall::check_and_clone_cstr(
            path,
            Some(crate::filesystem::vfs::MAX_PATHLEN),
        )?
        .into_string()
        .map_err(|_| SystemError::EINVAL)?;
        do_mkdir_at(dirfd, &path, ModeType::from_bits_truncate(mode as u32))?;
        Ok(0)
    }

    /// Formats the syscall arguments for display/debugging purposes.
    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("dirfd", format!("{:#x}", Self::dirfd(args))),
            FormattedSyscallParam::new("path", format!("{:#x}", Self::path(args) as usize)),
            FormattedSyscallParam::new("mode", format!("{:#x}", Self::mode(args))),
        ]
    }
}

impl SysMkdirAtHandle {
    /// Extracts the dirfd argument from syscall parameters.
    fn dirfd(args: &[usize]) -> i32 {
        args[0] as i32
    }
    /// Extracts the path argument from syscall parameters.
    fn path(args: &[usize]) -> *const u8 {
        args[1] as *const u8
    }
    /// Extracts the mode argument from syscall parameters.
    fn mode(args: &[usize]) -> usize {
        args[2]
    }
}

syscall_table_macros::declare_syscall!(SYS_MKDIRAT, SysMkdirAtHandle);
