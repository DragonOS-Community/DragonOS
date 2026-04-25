use system_error::SystemError;

use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_GETPEERNAME;
use crate::net::posix::SockAddr;
use crate::process::ProcessManager;
use crate::syscall::table::{FormattedSyscallParam, Syscall};
use alloc::string::ToString;
use alloc::vec::Vec;

/// System call handler for the `getpeername` syscall
///
/// This handler implements the `Syscall` trait to provide functionality for getting the peer address of a socket.
pub struct SysGetpeernameHandle;

impl Syscall for SysGetpeernameHandle {
    /// Returns the number of arguments expected by the `getpeername` syscall
    fn num_args(&self) -> usize {
        3
    }

    /// Handles the `getpeername` system call
    ///
    /// Returns the address of the peer connected to the socket.
    ///
    /// # Arguments
    /// * `args` - Array containing:
    ///   - args[0]: File descriptor (usize)
    ///   - args[1]: Address pointer (*mut SockAddr)
    ///   - args[2]: Address length pointer (*mut u32)
    /// * `frame` - Trap frame (not used)
    ///
    /// # Returns
    /// * `Ok(usize)` - 0 on success
    /// * `Err(SystemError)` - Error code if operation fails
    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let fd = Self::fd(args);
        let addr = Self::addr(args);
        let addrlen = Self::addrlen(args);

        do_getpeername(fd, addr, addrlen)
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
            FormattedSyscallParam::new("addr", format!("{:#x}", Self::addr(args) as usize)),
            FormattedSyscallParam::new("addrlen", format!("{:#x}", Self::addrlen(args) as usize)),
        ]
    }
}

impl SysGetpeernameHandle {
    /// Extracts the file descriptor from syscall arguments
    fn fd(args: &[usize]) -> usize {
        args[0]
    }

    /// Extracts the address pointer from syscall arguments
    fn addr(args: &[usize]) -> *mut SockAddr {
        args[1] as *mut SockAddr
    }

    /// Extracts the address length pointer from syscall arguments
    fn addrlen(args: &[usize]) -> *mut u32 {
        args[2] as *mut u32
    }
}

syscall_table_macros::declare_syscall!(SYS_GETPEERNAME, SysGetpeernameHandle);

/// Internal implementation of the getpeername operation
///
/// # Arguments
/// * `fd` - File descriptor
/// * `addr` - Address pointer
/// * `addrlen` - Address length pointer
///
/// # Returns
/// * `Ok(usize)` - 0 on success
/// * `Err(SystemError)` - Error code if operation fails
pub(super) fn do_getpeername(
    fd: usize,
    addr: *mut SockAddr,
    addrlen: *mut u32,
) -> Result<usize, SystemError> {
    if addr.is_null() {
        return Err(SystemError::EINVAL);
    }

    ProcessManager::current_pcb()
        .get_socket_inode(fd as i32)?
        .as_socket()
        .unwrap()
        .remote_endpoint()?
        .write_to_user(addr, addrlen)?;

    Ok(0)
}
