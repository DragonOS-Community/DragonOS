use system_error::SystemError;

use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_SENDMSG;
use crate::filesystem::vfs::{iov::IoVecs, IndexNode};
use crate::net::posix::{MsgHdr, SockAddr};
use crate::net::socket;
use crate::syscall::table::{FormattedSyscallParam, Syscall};
use crate::syscall::user_access::UserBufferReader;
use alloc::string::ToString;
use alloc::sync::Arc;
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

        do_sendmsg_user(fd, msg, flags, frame.is_from_user())
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

pub(super) fn do_sendmsg_user(
    fd: usize,
    msg: *const MsgHdr,
    flags: u32,
    from_user: bool,
) -> Result<usize, SystemError> {
    let (socket_inode, pmsg) = super::sys_sendto::prepare_send_socket(fd, flags)?;

    // Read MsgHdr from user space only after fd/socket validation, matching
    // Linux error ordering for bad fd plus bad user pointer.
    let msg_hdr = {
        let user_buffer_reader =
            UserBufferReader::new(msg, core::mem::size_of::<MsgHdr>(), from_user)?;
        user_buffer_reader.read_one_from_user::<MsgHdr>(0)?
    };

    do_sendmsg_prepared(&socket_inode, pmsg, &msg_hdr)
}

pub(super) fn do_sendmsg_prepared(
    socket_inode: &Arc<dyn IndexNode>,
    pmsg: socket::PMSG,
    msg: &MsgHdr,
) -> Result<usize, SystemError> {
    let socket = socket_inode.as_socket().ok_or(SystemError::ENOTSOCK)?;

    let endpoint = if msg.msg_name.is_null() {
        None
    } else {
        socket.validate_sendto_addr(msg.msg_name as *const SockAddr, msg.msg_namelen)?;
        Some(SockAddr::to_endpoint(
            msg.msg_name as *const SockAddr,
            msg.msg_namelen,
        )?)
    };

    // Prefer socket-level send_msg if implemented.
    match socket.send_msg(msg, pmsg) {
        Ok(n) => return Ok(n),
        Err(SystemError::ENOSYS) => {}
        Err(e) => return Err(e),
    }

    // Validate and parse iovecs, then gather user data.
    let iovs = unsafe { IoVecs::from_user(msg.msg_iov, msg.msg_iovlen, false)? };
    let buf = iovs.gather()?;
    if let Some(endpoint) = endpoint {
        socket.send_to(&buf, pmsg, endpoint)
    } else {
        socket.send(&buf, pmsg)
    }
}
