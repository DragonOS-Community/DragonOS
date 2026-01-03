use system_error::SystemError;

use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_RECVFROM;
use crate::filesystem::vfs::file::FileFlags;
use crate::mm::VirtAddr;
use crate::net::posix::SockAddr;
use crate::net::socket;
use crate::process::ProcessManager;
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

        // Verify buffer and address validity if from user space
        if frame.is_from_user() {
            let virt_buf = VirtAddr::new(buf as usize);
            if crate::mm::verify_area(virt_buf, len).is_err() {
                return Err(SystemError::EFAULT);
            }

            if !addrlen.is_null() {
                let virt_addrlen = VirtAddr::new(addrlen as usize);
                if crate::mm::verify_area(virt_addrlen, core::mem::size_of::<u32>()).is_err() {
                    return Err(SystemError::EFAULT);
                }
            }

            if !addr.is_null() {
                let virt_addr = VirtAddr::new(addr as usize);
                if crate::mm::verify_area(virt_addr, core::mem::size_of::<SockAddr>()).is_err() {
                    return Err(SystemError::EFAULT);
                }
            }
        }

        // Create mutable buffer slice
        let buf_slice = unsafe { core::slice::from_raw_parts_mut(buf, len) };

        do_recvfrom(fd, buf_slice, flags, addr, addrlen)
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
    buf: &mut [u8],
    flags: u32,
    addr: *mut SockAddr,
    addr_len: *mut u32,
) -> Result<usize, SystemError> {
    // Honor O_NONBLOCK set via fcntl(F_SETFL) by translating it to MSG_DONTWAIT.
    let file_nonblock = {
        let binding = ProcessManager::current_pcb().fd_table();
        let guard = binding.read();
        let file = guard.get_file_by_fd(fd as i32).ok_or(SystemError::EBADF)?;
        file.flags().contains(FileFlags::O_NONBLOCK)
    };

    let socket_inode = ProcessManager::current_pcb().get_socket_inode(fd as i32)?;
    let socket = socket_inode.as_socket().unwrap();

    let mut pmsg_flags = socket::PMSG::from_bits_truncate(flags);
    if file_nonblock {
        pmsg_flags.insert(socket::PMSG::DONTWAIT);
    }

    if addr.is_null() {
        let (n, _) = socket.recv_from(buf, pmsg_flags, None)?;
        return Ok(n);
    }

    // Linux 语义：recvfrom 的 addr/addrlen 是纯输出参数，内核不得读取 addr 缓冲区内容。
    // 用户栈上的 sockaddr 可能是未初始化的；读取它会导致错误解析并返回 EINVAL。
    if addr_len.is_null() {
        return Err(SystemError::EFAULT);
    }

    let (recv_len, endpoint) = socket.recv_from(buf, pmsg_flags, None)?;
    endpoint.write_to_user(addr, addr_len)?;
    Ok(recv_len)
}
