use system_error::SystemError;

use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_SENDTO;
use crate::filesystem::vfs::file::FileFlags;
use crate::mm::VirtAddr;
use crate::net::posix::SockAddr;
use crate::net::socket;
use crate::process::ProcessManager;
use crate::syscall::table::{FormattedSyscallParam, Syscall};
use crate::syscall::user_access::UserBufferReader;
use alloc::string::ToString;
use alloc::vec::Vec;

/// System call handler for the `sendto` syscall
///
/// This handler implements the `Syscall` trait to provide functionality for sending data to a socket.
pub struct SysSendtoHandle;

impl Syscall for SysSendtoHandle {
    /// Returns the number of arguments expected by the `sendto` syscall
    fn num_args(&self) -> usize {
        6
    }

    /// Handles the `sendto` system call
    ///
    /// Sends data to a socket, optionally to a specific address.
    ///
    /// # Arguments
    /// * `args` - Array containing:
    ///   - args[0]: File descriptor (usize)
    ///   - args[1]: Buffer pointer (*const u8)
    ///   - args[2]: Buffer length (usize)
    ///   - args[3]: Flags (u32)
    ///   - args[4]: Address pointer (*const SockAddr) - may be null
    ///   - args[5]: Address length (usize)
    /// * `frame` - Trap frame
    ///
    /// # Returns
    /// * `Ok(usize)` - Number of bytes sent
    /// * `Err(SystemError)` - Error code if operation fails
    fn handle(&self, args: &[usize], frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let fd = Self::fd(args);
        let buf = Self::buf(args);
        let len = Self::len(args);
        let flags = Self::flags(args);
        let addr = Self::addr(args);
        let addrlen = Self::addrlen(args);

        // Verify buffer and address validity if from user space
        if frame.is_from_user() {
            let virt_buf = VirtAddr::new(buf as usize);
            if crate::mm::verify_area(virt_buf, len).is_err() {
                return Err(SystemError::EFAULT);
            }

            if !addr.is_null() {
                let virt_addr = VirtAddr::new(addr as usize);
                if crate::mm::verify_area(virt_addr, addrlen).is_err() {
                    return Err(SystemError::EFAULT);
                }
            }
        }

        // Read data from user space
        let user_buffer_reader = UserBufferReader::new(buf, len, frame.is_from_user())?;
        let data = user_buffer_reader.read_from_user_checked(0)?;

        do_sendto(fd, data, flags, addr, addrlen as u32)
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
            FormattedSyscallParam::new("addrlen", Self::addrlen(args).to_string()),
        ]
    }
}

impl SysSendtoHandle {
    /// Extracts the file descriptor from syscall arguments
    fn fd(args: &[usize]) -> usize {
        args[0]
    }

    /// Extracts the buffer pointer from syscall arguments
    fn buf(args: &[usize]) -> *const u8 {
        args[1] as *const u8
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
    fn addr(args: &[usize]) -> *const SockAddr {
        args[4] as *const SockAddr
    }

    /// Extracts the address length from syscall arguments
    fn addrlen(args: &[usize]) -> usize {
        args[5]
    }
}

syscall_table_macros::declare_syscall!(SYS_SENDTO, SysSendtoHandle);

/// Internal implementation of the sendto operation
///
/// # Arguments
/// * `fd` - File descriptor
/// * `buf` - Buffer containing data to send
/// * `flags` - Flags
/// * `addr` - Address pointer (may be null)
/// * `addrlen` - Address length
///
/// # Returns
/// * `Ok(usize)` - Number of bytes sent
/// * `Err(SystemError)` - Error code if operation fails
pub(super) fn do_sendto(
    fd: usize,
    buf: &[u8],
    flags: u32,
    addr: *const SockAddr,
    addrlen: u32,
) -> Result<usize, SystemError> {
    // Honor O_NONBLOCK set via fcntl(F_SETFL) by translating it to MSG_DONTWAIT.
    let file_nonblock = {
        let binding = ProcessManager::current_pcb().fd_table();
        let guard = binding.read();
        let file = guard.get_file_by_fd(fd as i32).ok_or(SystemError::EBADF)?;
        file.flags().contains(FileFlags::O_NONBLOCK)
    };

    let endpoint = if addr.is_null() {
        None
    } else {
        Some(SockAddr::to_endpoint(addr, addrlen)?)
    };

    let mut pmsg_flags = socket::PMSG::from_bits_truncate(flags);
    if file_nonblock {
        pmsg_flags.insert(socket::PMSG::DONTWAIT);
    }

    let socket_inode = ProcessManager::current_pcb().get_socket_inode(fd as i32)?;
    let socket = socket_inode.as_socket().unwrap();

    if let Some(endpoint) = endpoint {
        socket.send_to(buf, pmsg_flags, endpoint)
    } else {
        socket.send(buf, pmsg_flags)
    }
}
