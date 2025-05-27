//! System call handler for opening files.

use system_error::SystemError;

use crate::arch::syscall::nr::SYS_OPEN;
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;

use alloc::string::ToString;
use alloc::vec::Vec;

/// Handler for the `open` system call.
pub struct SysOpenHandle;

impl Syscall for SysOpenHandle {
    /// Returns the number of arguments this syscall takes (3).
    fn num_args(&self) -> usize {
        3
    }

    /// Handles the open syscall by extracting arguments and calling `do_open`.
    fn handle(&self, args: &[usize], _from_user: bool) -> Result<usize, SystemError> {
        let path = Self::path(args);
        let flags = Self::flags(args);
        let mode = Self::mode(args);

        super::open_utils::do_open(path, flags, mode, true)
    }

    /// Formats the syscall arguments for display/debugging purposes.
    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("path", format!("{:#x}", Self::path(args) as usize)),
            FormattedSyscallParam::new("flags", Self::flags(args).to_string()),
            FormattedSyscallParam::new("mode", Self::mode(args).to_string()),
        ]
    }
}

impl SysOpenHandle {
    /// Extracts the path argument from syscall parameters.
    fn path(args: &[usize]) -> *const u8 {
        args[0] as *const u8
    }

    /// Extracts the flags argument from syscall parameters.
    fn flags(args: &[usize]) -> u32 {
        args[1] as u32
    }

    /// Extracts the mode argument from syscall parameters.
    fn mode(args: &[usize]) -> u32 {
        args[2] as u32
    }
}

syscall_table_macros::declare_syscall!(SYS_OPEN, SysOpenHandle);
