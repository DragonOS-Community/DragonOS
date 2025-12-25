use system_error::SystemError;

use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_ACCEPT;
use crate::filesystem::vfs::file::{File, FileFlags};
use crate::net::posix::SockAddr;
use crate::process::ProcessManager;
use crate::syscall::table::{FormattedSyscallParam, Syscall};
use alloc::string::ToString;
use alloc::vec::Vec;

/// System call handler for the `accept` syscall
///
/// This handler implements the `Syscall` trait to provide functionality for accepting a connection on a socket.
pub struct SysAcceptHandle;

impl Syscall for SysAcceptHandle {
    /// Returns the number of arguments expected by the `accept` syscall
    fn num_args(&self) -> usize {
        3
    }

    /// Handles the `accept` system call
    ///
    /// Accepts a connection on a socket.
    ///
    /// # Arguments
    /// * `args` - Array containing:
    ///   - args[0]: File descriptor (usize)
    ///   - args[1]: Address pointer (*mut SockAddr) - may be null
    ///   - args[2]: Address length pointer (*mut u32) - may be null
    /// * `frame` - Trap frame (not used)
    ///
    /// # Returns
    /// * `Ok(usize)` - File descriptor of the accepted socket
    /// * `Err(SystemError)` - Error code if operation fails
    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let fd = Self::fd(args);
        let addr = Self::addr(args);
        let addrlen = Self::addrlen(args);

        do_accept(fd, addr, addrlen, 0)
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

impl SysAcceptHandle {
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

syscall_table_macros::declare_syscall!(SYS_ACCEPT, SysAcceptHandle);

/// Internal implementation of the accept operation
///
/// This function is shared by both accept and accept4.
///
/// # Arguments
/// * `fd` - File descriptor
/// * `addr` - Address pointer (may be null)
/// * `addrlen` - Address length pointer (may be null)
/// * `flags` - Flags for accept4 (0 for accept)
///
/// # Returns
/// * `Ok(usize)` - File descriptor of the accepted socket
/// * `Err(SystemError)` - Error code if operation fails
pub(crate) fn do_accept(
    fd: usize,
    addr: *mut SockAddr,
    addrlen: *mut u32,
    flags: u32,
) -> Result<usize, SystemError> {
    let (new_socket, remote_endpoint) = {
        ProcessManager::current_pcb()
            .get_socket_inode(fd as i32)?
            .as_socket()
            .unwrap()
            .accept()?
    };

    let mut file_mode = FileFlags::O_RDWR;
    if flags & FileFlags::O_NONBLOCK.bits() != 0 {
        file_mode |= FileFlags::O_NONBLOCK;
    }
    if flags & FileFlags::O_CLOEXEC.bits() != 0 {
        file_mode |= FileFlags::O_CLOEXEC;
    }

    let new_fd = ProcessManager::current_pcb()
        .fd_table()
        .write()
        .alloc_fd(File::new(new_socket, file_mode)?, None)?;

    if !addr.is_null() {
        // 将对端地址写入用户空间
        remote_endpoint.write_to_user(addr, addrlen)?;
    }
    Ok(new_fd as usize)
}
