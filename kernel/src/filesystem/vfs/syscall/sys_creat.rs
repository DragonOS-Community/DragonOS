//! System call handler for creating files (creat).
//!
//! The `creat` system call is a legacy interface that is equivalent to:
//! `open(path, O_WRONLY | O_CREAT | O_TRUNC, mode)`
//!
//! This implementation follows Linux semantics:
//! - Creates a new file if it doesn't exist
//! - Truncates an existing file to zero length
//! - Opens the file in write-only mode
//! - Returns ENAMETOOLONG if the filename is too long

use system_error::SystemError;

use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_CREAT;
use crate::filesystem::vfs::file::FileFlags;
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;

use alloc::string::ToString;
use alloc::vec::Vec;

/// Handler for the `creat` system call.
pub struct SysCreatHandle;

impl Syscall for SysCreatHandle {
    /// Returns the number of arguments this syscall takes (2).
    fn num_args(&self) -> usize {
        2
    }

    /// Handles the creat syscall by extracting arguments and calling `do_open`
    /// with O_WRONLY | O_CREAT | O_TRUNC flags.
    ///
    /// # Arguments
    /// * `args[0]` - Path to the file (pointer to C string)
    /// * `args[1]` - File mode/permissions
    ///
    /// # Returns
    /// File descriptor on success, or error code on failure.
    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let path = Self::path(args);
        let mode = Self::mode(args);

        // creat(path, mode) is equivalent to open(path, O_WRONLY | O_CREAT | O_TRUNC, mode)
        let flags = (FileFlags::O_WRONLY | FileFlags::O_CREAT | FileFlags::O_TRUNC).bits();

        super::open_utils::do_open(path, flags, mode)
    }

    /// Formats the syscall arguments for display/debugging purposes.
    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("path", format!("{:#x}", Self::path(args) as usize)),
            FormattedSyscallParam::new("mode", Self::mode(args).to_string()),
        ]
    }
}

impl SysCreatHandle {
    /// Extracts the path argument from syscall parameters.
    fn path(args: &[usize]) -> *const u8 {
        args[0] as *const u8
    }

    /// Extracts the mode argument from syscall parameters.
    fn mode(args: &[usize]) -> u32 {
        args[1] as u32
    }
}

syscall_table_macros::declare_syscall!(SYS_CREAT, SysCreatHandle);
