use system_error::SystemError;

use super::socket_fd::SocketFdRef;
use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_RECVFROM;
use crate::mm::VirtAddr;
use crate::net::posix::SockAddr;
use crate::net::socket::PMSG;
use crate::net::socket::{endpoint::write_address_to_user, RecvFromAddrBehavior};
use crate::syscall::table::{FormattedSyscallParam, Syscall};
use alloc::string::ToString;
use alloc::vec::Vec;

/// System call handler for the `recvfrom` syscall
///
/// This handler implements the `Syscall` trait to provide functionality for receiving data from a socket.
pub struct SysRecvfromHandle;

impl Syscall for SysRecvfromHandle {
    /// Returns the number of arguments expected by the `recvfrom` syscall
    fn num_args(&self) -> usize {
        6
    }

    /// Handles the `recvfrom` system call
    ///
    /// Receives data from a socket, optionally storing the source address.
    ///
    /// # Arguments
    /// * `args` - Array containing:
    ///   - args[0]: File descriptor (usize)
    ///   - args[1]: Buffer pointer (*mut u8)
    ///   - args[2]: Buffer length (usize)
    ///   - args[3]: Flags (u32)
    ///   - args[4]: Address pointer (*mut SockAddr) - may be null
    ///   - args[5]: Address length pointer (*mut u32) - may be null
    /// * `frame` - Trap frame
    ///
    /// # Returns
    /// * `Ok(usize)` - Number of bytes received
    /// * `Err(SystemError)` - Error code if operation fails
    fn handle(&self, args: &[usize], frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let fd = Self::fd(args);
        let buf = Self::buf(args);
        let len = Self::len(args);
        let flags = Self::flags(args);
        let addr = Self::addr(args);
        let addrlen = Self::addrlen(args);

        // Import payload capacity before owner admission; output addresses are
        // copied only after receive consumption, as move_addr_to_user does.
        if frame.is_from_user() {
            let virt_buf = VirtAddr::new(buf as usize);
            if crate::mm::access_ok(virt_buf, len).is_err() {
                return Err(SystemError::EFAULT);
            }
        }

        // Keep only the address until the protocol selects/copies data. NULL
        // is valid for TCP TRUNC and for receives that never touch the payload.
        // SAFETY: the user range was checked above; owners perform the access.
        let mut buffer = unsafe {
            crate::syscall::user_buffer::UserBuffer::new(VirtAddr::new(buf as usize), len)
        };
        do_recvfrom(fd, &mut buffer, flags, addr, addrlen)
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
            FormattedSyscallParam::new("buf", format!("{:#x}", Self::buf(args) as usize)),
            FormattedSyscallParam::new("len", Self::len(args).to_string()),
            FormattedSyscallParam::new("flags", format!("{:#x}", Self::flags(args))),
            FormattedSyscallParam::new("addr", format!("{:#x}", Self::addr(args) as usize)),
            FormattedSyscallParam::new("addrlen", format!("{:#x}", Self::addrlen(args) as usize)),
        ]
    }
}

impl SysRecvfromHandle {
    /// Extracts the file descriptor from syscall arguments
    fn fd(args: &[usize]) -> usize {
        args[0]
    }

    /// Extracts the buffer pointer from syscall arguments
    fn buf(args: &[usize]) -> *mut u8 {
        args[1] as *mut u8
    }

    /// Extracts the buffer length from syscall arguments
    fn len(args: &[usize]) -> usize {
        args[2]
    }

    /// Extracts the flags from syscall arguments
    fn flags(args: &[usize]) -> u32 {
        args[3] as u32
    }

    /// Extracts the address pointer from syscall arguments
    fn addr(args: &[usize]) -> *mut SockAddr {
        args[4] as *mut SockAddr
    }

    /// Extracts the address length pointer from syscall arguments
    fn addrlen(args: &[usize]) -> *mut u32 {
        args[5] as *mut u32
    }
}

syscall_table_macros::declare_syscall!(SYS_RECVFROM, SysRecvfromHandle);

/// Internal implementation of the recvfrom operation
///
/// # Arguments
/// * `fd` - File descriptor
/// * `buf` - Buffer to receive data
/// * `flags` - Flags
/// * `addr` - Address pointer (may be null)
/// * `addr_len` - Address length pointer (may be null)
///
/// # Returns
/// * `Ok(usize)` - Number of bytes received
/// * `Err(SystemError)` - Error code if operation fails
pub(super) fn do_recvfrom(
    fd: usize,
    buf: &mut crate::syscall::user_buffer::UserBuffer<'_>,
    flags: u32,
    addr: *mut SockAddr,
    addr_len: *mut u32,
) -> Result<usize, SystemError> {
    // Retain the open file description while recv may block, even if another
    // thread closes or reuses the descriptor.
    let sock = SocketFdRef::from_fd(fd as i32)?;
    let socket = sock.socket()?;

    // Honor O_NONBLOCK set via fcntl(F_SETFL) by translating it to MSG_DONTWAIT.
    let mut pmsg_flags = PMSG::from_bits_retain(flags);
    if sock.is_nonblocking() {
        pmsg_flags.insert(PMSG::DONTWAIT);
    }

    let (recv_len, endpoint) = socket.recv_from_user_buffer(buf, pmsg_flags)?;
    // Linux ignores addr_len entirely when addr is NULL. Otherwise even an
    // empty protocol address result participates in move_addr_to_user's length
    // protocol, after the payload has been copied/consumed (or peeked).
    if !addr.is_null() {
        let address = match socket.recvfrom_addr_behavior(pmsg_flags) {
            RecvFromAddrBehavior::Write => Some(&endpoint),
            RecvFromAddrBehavior::Ignore => None,
        };
        write_address_to_user(address, addr, addr_len)?;
    }

    Ok(recv_len)
}
