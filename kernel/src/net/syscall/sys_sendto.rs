use system_error::SystemError;

use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_SENDTO;
use crate::filesystem::vfs::{file::FileFlags, FileType, IndexNode};
use crate::mm::VirtAddr;
use crate::net::posix::SockAddr;
use crate::net::socket;
use crate::net::socket::endpoint::Endpoint;
use crate::process::ProcessManager;
use crate::syscall::table::{FormattedSyscallParam, Syscall};
use crate::syscall::user_access::UserBufferReader;
use alloc::string::ToString;
use alloc::sync::Arc;
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

        // Match Linux import_single_range(): reject out-of-range user buffers
        // before fd lookup, but do not fault in or copy the payload here.
        if frame.is_from_user() {
            let virt_buf = VirtAddr::new(buf as usize);
            if crate::mm::access_ok(virt_buf, len).is_err() {
                return Err(SystemError::EFAULT);
            }
        }

        let prepared = prepare_send_common(fd, flags, addr, addrlen as u32)?;

        // Pass the validated user payload to the socket layer. Stream sockets
        // can consume it in bounded chunks; message-oriented sockets keep their
        // atomic message copy after length validation.
        let user_buffer_reader = UserBufferReader::new(buf, len, frame.is_from_user())?;

        do_sendto_user_prepared(&prepared, &user_buffer_reader, len)
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

pub(super) struct PreparedSend {
    pub(super) socket_inode: Arc<dyn IndexNode>,
    pub(super) pmsg_flags: socket::PMSG,
    endpoint: Option<Endpoint>,
}

pub(super) fn prepare_send_common(
    fd: usize,
    flags: u32,
    addr: *const SockAddr,
    addrlen: u32,
) -> Result<PreparedSend, SystemError> {
    let (socket_inode, pmsg_flags) = prepare_send_socket(fd, flags)?;

    let socket = socket_inode.as_socket().ok_or(SystemError::ENOTSOCK)?;
    let endpoint = if addr.is_null() {
        None
    } else {
        socket.validate_sendto_addr(addr, addrlen)?;
        Some(SockAddr::to_endpoint(addr, addrlen)?)
    };

    Ok(PreparedSend {
        socket_inode,
        pmsg_flags,
        endpoint,
    })
}

pub(super) fn prepare_send_socket(
    fd: usize,
    flags: u32,
) -> Result<(Arc<dyn IndexNode>, socket::PMSG), SystemError> {
    let (socket_inode, file_nonblock) = {
        let binding = ProcessManager::current_pcb().fd_table();
        let guard = binding.read();
        let file = guard.get_file_by_fd(fd as i32).ok_or(SystemError::EBADF)?;
        if file.file_type() != FileType::Socket {
            return Err(SystemError::ENOTSOCK);
        }
        (file.inode(), file.flags().contains(FileFlags::O_NONBLOCK))
    };

    socket_inode.as_socket().ok_or(SystemError::ENOTSOCK)?;

    let mut pmsg_flags = socket::PMSG::from_bits_truncate(flags);
    if file_nonblock {
        pmsg_flags.insert(socket::PMSG::DONTWAIT);
    }

    Ok((socket_inode, pmsg_flags))
}

pub(super) fn do_sendto_user_prepared(
    prepared: &PreparedSend,
    reader: &UserBufferReader<'_>,
    len: usize,
) -> Result<usize, SystemError> {
    let socket = prepared.socket_inode.as_socket().unwrap();
    socket.send_user_buffer(reader, len, prepared.pmsg_flags, prepared.endpoint.clone())
}
