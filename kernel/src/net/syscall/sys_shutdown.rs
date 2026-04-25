use system_error::SystemError;

use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_SHUTDOWN;
use crate::net::socket::common::ShutdownBit;
use crate::process::ProcessManager;
use crate::syscall::table::{FormattedSyscallParam, Syscall};
use alloc::string::ToString;
use alloc::vec::Vec;

/// System call handler for the `shutdown` syscall
///
/// This handler implements the `Syscall` trait to provide functionality for shutting down a socket.
pub struct SysShutdownHandle;

impl Syscall for SysShutdownHandle {
    /// Returns the number of arguments expected by the `shutdown` syscall
    fn num_args(&self) -> usize {
        2
    }

    /// Handles the `shutdown` system call
    ///
    /// Shuts down part of a full-duplex connection.
    ///
    /// # Arguments
    /// * `args` - Array containing:
    ///   - args[0]: File descriptor (usize)
    ///   - args[1]: How (usize) - shutdown direction
    /// * `frame` - Trap frame (not used)
    ///
    /// # Returns
    /// * `Ok(usize)` - 0 on success
    /// * `Err(SystemError)` - Error code if operation fails
    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let fd = Self::fd(args);
        let how = Self::how(args);

        do_shutdown(fd, how)
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
            FormattedSyscallParam::new("how", Self::how(args).to_string()),
        ]
    }
}

impl SysShutdownHandle {
    /// Extracts the file descriptor from syscall arguments
    fn fd(args: &[usize]) -> usize {
        args[0]
    }

    /// Extracts the how parameter from syscall arguments
    fn how(args: &[usize]) -> usize {
        args[1]
    }
}

syscall_table_macros::declare_syscall!(SYS_SHUTDOWN, SysShutdownHandle);

/// Internal implementation of the shutdown operation
///
/// # Arguments
/// * `fd` - File descriptor
/// * `how` - Shutdown direction
///
/// # Returns
/// * `Ok(usize)` - 0 on success
/// * `Err(SystemError)` - Error code if operation fails
pub(super) fn do_shutdown(fd: usize, how: usize) -> Result<usize, SystemError> {
    ProcessManager::current_pcb()
        .get_socket_inode(fd as i32)?
        .as_socket()
        .unwrap()
        .shutdown(ShutdownBit::try_from(how)?)
        .map(|()| 0)
}
