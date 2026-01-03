use system_error::SystemError;

use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_LISTEN;
use crate::process::ProcessManager;
use crate::syscall::table::{FormattedSyscallParam, Syscall};
use alloc::string::ToString;
use alloc::vec::Vec;

/// System call handler for the `listen` syscall
///
/// This handler implements the `Syscall` trait to provide functionality for listening on a socket.
pub struct SysListenHandle;

impl Syscall for SysListenHandle {
    /// Returns the number of arguments expected by the `listen` syscall
    fn num_args(&self) -> usize {
        2
    }

    /// Handles the `listen` system call
    ///
    /// Marks a socket as listening for incoming connections.
    ///
    /// # Arguments
    /// * `args` - Array containing:
    ///   - args[0]: File descriptor (usize)
    ///   - args[1]: Backlog (usize)
    /// * `frame` - Trap frame (not used)
    ///
    /// # Returns
    /// * `Ok(usize)` - 0 on success
    /// * `Err(SystemError)` - Error code if operation fails
    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let fd = Self::fd(args);
        let backlog = Self::backlog(args);

        do_listen(fd, backlog)
    }

    /// Formats the syscall parameters for display/debug purposes
    ///
    /// # Arguments
    /// * `args` - The raw syscall arguments
    ///
    /// # Returns
    /// Vector of formatted parameters with descriptive names
    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("fd", Self::fd(args).to_string()),
            FormattedSyscallParam::new("backlog", Self::backlog(args).to_string()),
        ]
    }
}

impl SysListenHandle {
    /// Extracts the file descriptor from syscall arguments
    fn fd(args: &[usize]) -> usize {
        args[0]
    }

    /// Extracts the backlog from syscall arguments
    fn backlog(args: &[usize]) -> usize {
        args[1]
    }
}

syscall_table_macros::declare_syscall!(SYS_LISTEN, SysListenHandle);

/// Internal implementation of the listen operation
///
/// # Arguments
/// * `fd` - File descriptor
/// * `backlog` - Maximum queue length
///
/// # Returns
/// * `Ok(usize)` - 0 on success
/// * `Err(SystemError)` - Error code if operation fails
pub(super) fn do_listen(fd: usize, backlog: usize) -> Result<usize, SystemError> {
    ProcessManager::current_pcb()
        .get_socket_inode(fd as i32)?
        .as_socket()
        .unwrap()
        .listen(backlog)
        .map(|_| 0)
}
