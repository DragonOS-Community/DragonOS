use system_error::SystemError;

use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_ACCEPT4;
use crate::filesystem::vfs::file::FileFlags;
use crate::net::posix::SockAddr;
use crate::syscall::table::{FormattedSyscallParam, Syscall};
use alloc::string::ToString;
use alloc::vec::Vec;

use super::sys_accept::do_accept;

/// System call handler for the `accept4` syscall
///
/// This handler implements the `Syscall` trait to provide functionality for accepting a connection on a socket with flags.
pub struct SysAccept4Handle;

impl Syscall for SysAccept4Handle {
    /// Returns the number of arguments expected by the `accept4` syscall
    fn num_args(&self) -> usize {
        4
    }

    /// Handles the `accept4` system call
    ///
    /// Accepts a connection on a socket with flags.
    ///
    /// # Arguments
    /// * `args` - Array containing:
    ///   - args[0]: File descriptor (usize)
    ///   - args[1]: Address pointer (*mut SockAddr) - may be null
    ///   - args[2]: Address length pointer (*mut u32) - may be null
    ///   - args[3]: Flags (u32)
    /// * `frame` - Trap frame (not used)
    ///
    /// # Returns
    /// * `Ok(usize)` - File descriptor of the accepted socket
    /// * `Err(SystemError)` - Error code if operation fails
    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let fd = Self::fd(args);
        let addr = Self::addr(args);
        let addrlen = Self::addrlen(args);
        let flags = Self::flags(args);

        do_accept4(fd, addr, addrlen, flags)
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
            FormattedSyscallParam::new("flags", format!("{:#x}", Self::flags(args))),
        ]
    }
}

impl SysAccept4Handle {
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

    /// Extracts the flags from syscall arguments
    fn flags(args: &[usize]) -> u32 {
        args[3] as u32
    }
}

syscall_table_macros::declare_syscall!(SYS_ACCEPT4, SysAccept4Handle);

/// Internal implementation of the accept4 operation
///
/// # Arguments
/// * `fd` - File descriptor
/// * `addr` - Address pointer (may be null)
/// * `addrlen` - Address length pointer (may be null)
/// * `flags` - Flags (SOCK_NONBLOCK, SOCK_CLOEXEC)
///
/// # Returns
/// * `Ok(usize)` - File descriptor of the accepted socket
/// * `Err(SystemError)` - Error code if operation fails
pub(super) fn do_accept4(
    fd: usize,
    addr: *mut SockAddr,
    addrlen: *mut u32,
    flags: u32,
) -> Result<usize, SystemError> {
    // 如果flags不合法，返回错误
    if (flags & (!(FileFlags::O_CLOEXEC | FileFlags::O_NONBLOCK)).bits()) != 0 {
        return Err(SystemError::EINVAL);
    }

    do_accept(fd, addr, addrlen, flags)
}
