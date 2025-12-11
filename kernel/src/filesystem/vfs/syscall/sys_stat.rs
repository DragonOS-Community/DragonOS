//! System call handler for opening files.

use system_error::SystemError;

use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_STAT;
use crate::filesystem::vfs::file::FileFlags;
use crate::filesystem::vfs::syscall::newfstat::do_newfstat;
use crate::filesystem::vfs::syscall::sys_close::do_close;
use crate::filesystem::vfs::InodeMode;
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;
use defer::defer;

use alloc::vec::Vec;

pub struct SysStatHandle;

impl Syscall for SysStatHandle {
    /// Returns the number of arguments this syscall takes.
    fn num_args(&self) -> usize {
        2
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let path = Self::path(args);
        let usr_kstat = Self::usr_kstat(args);

        let fd = super::open_utils::do_open(
            path,
            FileFlags::O_RDONLY.bits(),
            InodeMode::empty().bits(),
        )?;

        defer!({
            do_close(fd as i32).ok();
        });

        do_newfstat(fd as i32, usr_kstat)?;

        return Ok(0);
    }

    /// Formats the syscall arguments for display/debugging purposes.
    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("path", format!("{:#x}", Self::path(args) as usize)),
            FormattedSyscallParam::new("statbuf", format!("{:#x}", Self::usr_kstat(args))),
        ]
    }
}

impl SysStatHandle {
    /// Extracts the path argument from syscall parameters.
    fn path(args: &[usize]) -> *const u8 {
        args[0] as *const u8
    }

    /// Extracts the usr_kstat argument from syscall parameters.
    fn usr_kstat(args: &[usize]) -> usize {
        args[1]
    }
}

syscall_table_macros::declare_syscall!(SYS_STAT, SysStatHandle);
