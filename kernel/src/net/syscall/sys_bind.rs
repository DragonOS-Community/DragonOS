use system_error::SystemError;

use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_BIND;
use crate::mm::VirtAddr;
use crate::net::posix::SockAddr;
use crate::net::socket::endpoint::Endpoint;
use crate::process::ProcessManager;
use crate::syscall::table::{FormattedSyscallParam, Syscall};
use alloc::string::ToString;
use alloc::vec::Vec;

/// System call handler for the `bind` syscall
///
/// This handler implements the `Syscall` trait to provide functionality for binding a socket to an address.
pub struct SysBindHandle;

impl Syscall for SysBindHandle {
    /// Returns the number of arguments expected by the `bind` syscall
    fn num_args(&self) -> usize {
        3
    }

    /// Handles the `bind` system call
    ///
    /// Binds a socket to a local address.
    ///
    /// # Arguments
    /// * `args` - Array containing:
    ///   - args[0]: File descriptor (usize)
    ///   - args[1]: Address pointer (*const SockAddr)
    ///   - args[2]: Address length (usize)
    /// * `frame` - Trap frame
    ///
    /// # Returns
    /// * `Ok(usize)` - 0 on success
    /// * `Err(SystemError)` - Error code if operation fails
    fn handle(&self, args: &[usize], frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let fd = Self::fd(args);
        let addr = Self::addr(args);
        let addrlen = Self::addrlen(args);

        // Verify address validity if from user space
        if frame.is_from_user() {
            let virt_addr = VirtAddr::new(addr as usize);
            if crate::mm::verify_area(virt_addr, addrlen).is_err() {
                return Err(SystemError::EFAULT);
            }
        }

        do_bind(fd, addr, addrlen as u32)
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
            FormattedSyscallParam::new("addrlen", Self::addrlen(args).to_string()),
        ]
    }
}

impl SysBindHandle {
    /// Extracts the file descriptor from syscall arguments
    fn fd(args: &[usize]) -> usize {
        args[0]
    }

    /// Extracts the address pointer from syscall arguments
    fn addr(args: &[usize]) -> *const SockAddr {
        args[1] as *const SockAddr
    }

    /// Extracts the address length from syscall arguments
    fn addrlen(args: &[usize]) -> usize {
        args[2]
    }
}

syscall_table_macros::declare_syscall!(SYS_BIND, SysBindHandle);

/// Internal implementation of the bind operation
///
/// # Arguments
/// * `fd` - File descriptor
/// * `addr` - Address pointer
/// * `addrlen` - Address length
///
/// # Returns
/// * `Ok(usize)` - 0 on success
/// * `Err(SystemError)` - Error code if operation fails
pub(super) fn do_bind(
    fd: usize,
    addr: *const SockAddr,
    addrlen: u32,
) -> Result<usize, SystemError> {
    let endpoint: Endpoint = SockAddr::to_endpoint(addr, addrlen)?;
    ProcessManager::current_pcb()
        .get_socket_inode(fd as i32)?
        .as_socket()
        .unwrap()
        .bind(endpoint)?;
    Ok(0)
}
