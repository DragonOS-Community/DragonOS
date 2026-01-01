use system_error::SystemError;

use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_RECVMSG;
use crate::filesystem::vfs::{file::FileFlags, iov::IoVecs};
use crate::net::posix::MsgHdr;
use crate::net::socket;
use crate::process::ProcessManager;
use crate::syscall::table::{FormattedSyscallParam, Syscall};
use crate::syscall::user_access::{UserBufferReader, UserBufferWriter};
use alloc::string::ToString;
use alloc::vec::Vec;

/// System call handler for the `recvmsg` syscall
///
/// This handler implements the `Syscall` trait to provide functionality for receiving a message from a socket.
pub struct SysRecvmsgHandle;

impl Syscall for SysRecvmsgHandle {
    /// Returns the number of arguments expected by the `recvmsg` syscall
    fn num_args(&self) -> usize {
        3
    }

    /// Handles the `recvmsg` system call
    ///
    /// Receives a message from a socket.
    ///
    /// # Arguments
    /// * `args` - Array containing:
    ///   - args[0]: File descriptor (usize)
    ///   - args[1]: Message header pointer (*mut MsgHdr)
    ///   - args[2]: Flags (u32)
    /// * `frame` - Trap frame
    ///
    /// # Returns
    /// * `Ok(usize)` - Number of bytes received
    /// * `Err(SystemError)` - Error code if operation fails
    fn handle(&self, args: &[usize], frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let fd = Self::fd(args);
        let msg = Self::msg(args);
        let flags = Self::flags(args);

        do_recvmsg(fd, msg, flags, frame.is_from_user())
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

impl SysRecvmsgHandle {
    /// Extracts the file descriptor from syscall arguments
    fn fd(args: &[usize]) -> usize {
        args[0]
    }

    /// Extracts the message header pointer from syscall arguments
    fn msg(args: &[usize]) -> *mut MsgHdr {
        args[1] as *mut MsgHdr
    }

    /// Extracts the flags from syscall arguments
    fn flags(args: &[usize]) -> u32 {
        args[2] as u32
    }
}

syscall_table_macros::declare_syscall!(SYS_RECVMSG, SysRecvmsgHandle);

/// Internal implementation of the recvmsg operation
///
/// # Arguments
/// * `fd` - File descriptor
/// * `msg` - Message header pointer (user space)
/// * `flags` - Flags
///
/// # Returns
/// * `Ok(usize)` - Number of bytes received
/// * `Err(SystemError)` - Error code if operation fails
pub(super) fn do_recvmsg(
    fd: usize,
    msg: *mut MsgHdr,
    flags: u32,
    from_user: bool,
) -> Result<usize, SystemError> {
    if msg.is_null() {
        return Err(SystemError::EFAULT);
    }

    // Copy msghdr in once to avoid TOCTOU on pointers/lengths.
    let reader = UserBufferReader::new(msg, core::mem::size_of::<MsgHdr>(), from_user)?;
    let mut kmsg = reader.buffer_protected(0)?.read_one::<MsgHdr>(0)?;

    // log::debug!(
    //     "do_recvmsg: fd={}, msg_iovlen={}, msg_iov={:?}, flags={:#x}",
    //     fd,
    //     kmsg.msg_iovlen,
    //     kmsg.msg_iov,
    //     flags
    // );

    // 检查每个缓冲区地址是否合法，生成iovecs（fallback path needs this).
    let iovs = unsafe { IoVecs::from_user(kmsg.msg_iov, kmsg.msg_iovlen, true)? };

    // Honor O_NONBLOCK set via fcntl(F_SETFL) by translating it to MSG_DONTWAIT.
    let file_nonblock = {
        let binding = ProcessManager::current_pcb().fd_table();
        let guard = binding.read();
        let file = guard.get_file_by_fd(fd as i32).ok_or(SystemError::EBADF)?;
        file.flags().contains(FileFlags::O_NONBLOCK)
    };

    let (buf, recv_size, used_recv_msg) = {
        let socket_inode = ProcessManager::current_pcb().get_socket_inode(fd as i32)?;
        let socket = socket_inode.as_socket().unwrap();

        let mut pmsg_flags = socket::PMSG::from_bits_truncate(flags);
        if file_nonblock {
            pmsg_flags.insert(socket::PMSG::DONTWAIT);
        }

        // 优先使用 recv_msg 以便实现 msg_flags/msg_controllen 等语义。
        match socket.recv_msg(&mut kmsg, pmsg_flags) {
            Ok(recv_size) => (alloc::vec::Vec::new(), recv_size, true),
            Err(SystemError::ENOSYS) => {
                let mut buf = iovs.new_buf(true);
                // 从socket中读取数据
                let recv_size = socket.recv(&mut buf, pmsg_flags)?;
                (buf, recv_size, false)
            }
            Err(e) => return Err(e),
        }
    };

    // 将数据写入用户空间的iovecs（recv_msg 路径已自行处理散布写入）
    if !buf.is_empty() {
        iovs.scatter(&buf[..recv_size])?;
    }

    // Fallback path does not produce control messages.
    if !used_recv_msg {
        // 最小保证：不产生控制消息时必须把 msg_controllen 写回 0
        // 否则用户态 CMSG_FIRSTHDR 可能非空。
        kmsg.msg_controllen = 0;
        kmsg.msg_flags = 0;
    }

    // Copy out only the result fields that Linux updates.
    let mut writer =
        UserBufferWriter::new(msg as *mut u8, core::mem::size_of::<MsgHdr>(), from_user)?;
    let mut out = writer.buffer_protected(0)?;

    let namelen_off = core::mem::offset_of!(MsgHdr, msg_namelen);
    out.write_one::<u32>(namelen_off, &kmsg.msg_namelen)?;

    let controllen_off = core::mem::offset_of!(MsgHdr, msg_controllen);
    out.write_one::<usize>(controllen_off, &kmsg.msg_controllen)?;

    let flags_off = core::mem::offset_of!(MsgHdr, msg_flags);
    out.write_one::<i32>(flags_off, &kmsg.msg_flags)?;

    Ok(recv_size)
}
