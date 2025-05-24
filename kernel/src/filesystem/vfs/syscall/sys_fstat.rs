//! System call handler for opening files.

use system_error::SystemError;

use crate::arch::syscall::nr::SYS_FSTAT;
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;

use alloc::string::ToString;
use alloc::vec::Vec;

pub struct SysFstatHandle;

impl Syscall for SysFstatHandle {
    /// Returns the number of arguments this syscall takes.
    fn num_args(&self) -> usize {
        2
    }

    fn handle(&self, args: &[usize], _from_user: bool) -> Result<usize, SystemError> {
        let fd = Self::fd(args);
        let usr_kstat = Self::usr_kstat(args);
        crate::syscall::Syscall::newfstat(fd, usr_kstat)
    }

    /// Formats the syscall arguments for display/debugging purposes.
    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("fd", Self::fd(args).to_string()),
            FormattedSyscallParam::new("statbuf", format!("{:#x}", Self::usr_kstat(args))),
        ]
    }
}

impl SysFstatHandle {
    /// Extracts the fd argument from syscall parameters.
    fn fd(args: &[usize]) -> i32 {
        args[0] as i32
    }

    /// Extracts the usr_kstat argument from syscall parameters.
    fn usr_kstat(args: &[usize]) -> usize {
        args[1]
    }
}

syscall_table_macros::declare_syscall!(SYS_FSTAT, SysFstatHandle);
