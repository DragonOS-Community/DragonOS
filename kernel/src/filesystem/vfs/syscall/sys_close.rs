//! System call handler for closing files.

use alloc::string::ToString;

use crate::arch::syscall::nr::SYS_CLOSE;
use crate::process::ProcessManager;
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;
use alloc::vec::Vec;
use system_error::SystemError;

/// Handler for the `close` system call.
pub struct SysCloseHandle;

impl Syscall for SysCloseHandle {
    /// Returns the number of arguments this syscall takes (1).
    fn num_args(&self) -> usize {
        1
    }

    /// Handles the close syscall by extracting arguments and calling `do_close`.
    fn handle(&self, args: &[usize], _from_user: bool) -> Result<usize, SystemError> {
        let fd = Self::fd(args);
        do_close(fd)
    }
    /// Formats the syscall arguments for display/debugging purposes.
    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![FormattedSyscallParam::new("fd", Self::fd(args).to_string())]
    }
}

impl SysCloseHandle {
    /// Extracts the file descriptor (fd) argument from syscall parameters.
    fn fd(args: &[usize]) -> i32 {
        args[0] as i32
    }
}

syscall_table_macros::declare_syscall!(SYS_CLOSE, SysCloseHandle);

/// Close a file descriptor
///
/// # Arguments
/// - `fd`: The file descriptor to close
///
/// # Returns
/// Returns Ok(0) on success, or Err(SystemError) on failure
pub(super) fn do_close(fd: i32) -> Result<usize, SystemError> {
    let binding = ProcessManager::current_pcb().fd_table();
    let mut fd_table_guard = binding.write();
    let _file = fd_table_guard.drop_fd(fd)?;
    drop(fd_table_guard);
    Ok(0)
}
