use system_error::SystemError;

use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_SENDMSG;
use crate::filesystem::vfs::{file::FileFlags, iov::IoVecs};
use crate::net::posix::{MsgHdr, SockAddr};
use crate::net::socket;
use crate::process::ProcessManager;
use crate::syscall::table::{FormattedSyscallParam, Syscall};
use crate::syscall::user_access::UserBufferReader;
use alloc::string::ToString;
use alloc::vec::Vec;

/// System call handler for the `sendmsg` syscall
///
/// This handler implements the `Syscall` trait to provide functionality for sending a message on a socket.
pub struct SysSendmsgHandle;

impl Syscall for SysSendmsgHandle {
    /// Returns the number of arguments expected by the `sendmsg` syscall
    fn num_args(&self) -> usize {
        3
    }

    /// Handles the `sendmsg` system call
    ///
    /// Sends a message on a socket.
    ///
    /// # Arguments
    /// * `args` - Array containing:
    ///   - args[0]: File descriptor (usize)
    ///   - args[1]: Message header pointer (*const MsgHdr)
    ///   - args[2]: Flags (u32)
    /// * `frame` - Trap frame
    ///
    /// # Returns
    /// * `Ok(usize)` - Number of bytes sent
    /// * `Err(SystemError)` - Error code if operation fails
    fn handle(&self, args: &[usize], frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let fd = Self::fd(args);
        let msg = Self::msg(args);
        let flags = Self::flags(args);

        // Read MsgHdr from user space
        let user_buffer_reader =
            UserBufferReader::new(msg, core::mem::size_of::<MsgHdr>(), frame.is_from_user())?;
        let msg_hdr = user_buffer_reader.read_one_from_user::<MsgHdr>(0)?;

        do_sendmsg(fd, msg_hdr, flags)
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
            FormattedSyscallParam::new("msg", format!("{:#x}", Self::msg(args) as usize)),
            FormattedSyscallParam::new("flags", format!("{:#x}", Self::flags(args))),
        ]
    }
}

impl SysSendmsgHandle {
    /// Extracts the file descriptor from syscall arguments
    fn fd(args: &[usize]) -> usize {
        args[0]
    }

    /// Extracts the message header pointer from syscall arguments
    fn msg(args: &[usize]) -> *const MsgHdr {
        args[1] as *const MsgHdr
    }

    /// Extracts the flags from syscall arguments
    fn flags(args: &[usize]) -> u32 {
        args[2] as u32
    }
}

syscall_table_macros::declare_syscall!(SYS_SENDMSG, SysSendmsgHandle);

/// Internal implementation of the sendmsg operation
///
/// # Arguments
/// * `fd` - File descriptor
/// * `msg` - Message header
/// * `flags` - Flags
///
/// # Returns
/// * `Ok(usize)` - Number of bytes sent
/// * `Err(SystemError)` - Error code if operation fails
pub(super) fn do_sendmsg(fd: usize, msg: &MsgHdr, flags: u32) -> Result<usize, SystemError> {
    // Validate and parse iovecs, then gather user data.
    let iovs = unsafe { IoVecs::from_user(msg.msg_iov, msg.msg_iovlen, false)? };

    // Honor O_NONBLOCK set via fcntl(F_SETFL) by translating it to MSG_DONTWAIT.
    let file_nonblock = {
        let binding = ProcessManager::current_pcb().fd_table();
        let guard = binding.read();
        let file = guard.get_file_by_fd(fd as i32).ok_or(SystemError::EBADF)?;
        file.flags().contains(FileFlags::O_NONBLOCK)
    };

    let mut pmsg = socket::PMSG::from_bits_truncate(flags);
    if file_nonblock {
        pmsg.insert(socket::PMSG::DONTWAIT);
    }

    let endpoint = if msg.msg_name.is_null() {
        None
    } else {
        Some(SockAddr::to_endpoint(
            msg.msg_name as *const SockAddr,
            msg.msg_namelen,
        )?)
    };

    let socket_inode = ProcessManager::current_pcb().get_socket_inode(fd as i32)?;
    let socket = socket_inode.as_socket().unwrap();

    // Prefer socket-level send_msg if implemented.
    match socket.send_msg(msg, pmsg) {
        Ok(n) => return Ok(n),
        Err(SystemError::ENOSYS) => {}
        Err(e) => return Err(e),
    }

    let buf = iovs.gather()?;
    if let Some(endpoint) = endpoint {
        socket.send_to(&buf, pmsg, endpoint)
    } else {
        socket.send(&buf, pmsg)
    }
}
