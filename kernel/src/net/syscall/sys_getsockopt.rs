use system_error::SystemError;

use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_GETSOCKOPT;
use crate::arch::MMArch;
use crate::bpf::classic::{SockFilter, BPF_MAXINSNS};
use crate::mm::MemoryManagementArch;
use crate::net::socket::inet::stream::TcpOption;
use crate::net::socket::{PSO, PSOL};
use crate::process::ProcessManager;
use crate::syscall::table::{FormattedSyscallParam, Syscall};
use crate::syscall::user_access::{UserBufferReader, UserBufferWriter};
use alloc::string::ToString;
use alloc::vec::Vec;

/// getsockopt optval 最大长度限制（一页）
const MAX_OPTVAL_LEN: usize = MMArch::PAGE_SIZE;
const NETLINK_LIST_MEMBERSHIPS: usize = 9;

/// 计算实际拷贝长度：若 optval 为 null 则返回 need，否则返回 min(user_len, need)
#[inline]
fn calc_out_len(optval: *mut u8, user_len: usize, need: usize) -> usize {
    if optval.is_null() {
        need
    } else {
        core::cmp::min(user_len, need)
    }
}

/// System call handler for the `getsockopt` syscall
///
/// This handler implements the `Syscall` trait to provide functionality for getting socket options.
pub struct SysGetsockoptHandle;

impl Syscall for SysGetsockoptHandle {
    /// Returns the number of arguments expected by the `getsockopt` syscall
    fn num_args(&self) -> usize {
        5
    }

    /// Handles the `getsockopt` system call
    ///
    /// Gets a socket option.
    ///
    /// # Arguments
    /// * `args` - Array containing:
    ///   - args[0]: File descriptor (usize)
    ///   - args[1]: Level (usize)
    ///   - args[2]: Option name (usize)
    ///   - args[3]: Option value pointer (*mut u8) - may be null
    ///   - args[4]: Option length pointer (*mut u32)
    /// * `frame` - Trap frame
    ///
    /// # Returns
    /// * `Ok(usize)` - 0 on success
    /// * `Err(SystemError)` - Error code if operation fails
    fn handle(&self, args: &[usize], frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let fd = Self::fd(args);
        let level = Self::level(args);
        let optname = Self::optname(args);
        let optval = Self::optval(args);
        let optlen = Self::optlen(args);

        do_getsockopt(fd, level, optname, optval, optlen, frame.is_from_user())
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
            FormattedSyscallParam::new("level", Self::level(args).to_string()),
            FormattedSyscallParam::new("optname", Self::optname(args).to_string()),
            FormattedSyscallParam::new("optval", format!("{:#x}", Self::optval(args) as usize)),
            FormattedSyscallParam::new("optlen", format!("{:#x}", Self::optlen(args) as usize)),
        ]
    }
}

impl SysGetsockoptHandle {
    /// Extracts the file descriptor from syscall arguments
    fn fd(args: &[usize]) -> usize {
        args[0]
    }

    /// Extracts the level from syscall arguments
    fn level(args: &[usize]) -> usize {
        args[1]
    }

    /// Extracts the option name from syscall arguments
    fn optname(args: &[usize]) -> usize {
        args[2]
    }

    /// Extracts the option value pointer from syscall arguments
    fn optval(args: &[usize]) -> *mut u8 {
        args[3] as *mut u8
    }

    /// Extracts the option length pointer from syscall arguments
    fn optlen(args: &[usize]) -> *mut u32 {
        args[4] as *mut u32
    }
}

syscall_table_macros::declare_syscall!(SYS_GETSOCKOPT, SysGetsockoptHandle);

/// Internal implementation of the getsockopt operation
///
/// # Arguments
/// * `fd` - File descriptor
/// * `level` - Option level
/// * `optname` - Option name
/// * `optval` - Option value pointer (may be null)
/// * `optlen` - Option length pointer
///
/// # Returns
/// * `Ok(usize)` - 0 on success
/// * `Err(SystemError)` - Error code if operation fails
pub(super) fn do_getsockopt(
    fd: usize,
    level: usize,
    optname: usize,
    optval: *mut u8,
    optlen: *mut u32,
    from_user: bool,
) -> Result<usize, SystemError> {
    // 参数合法性检查
    if optlen.is_null() {
        return Err(SystemError::EFAULT);
    }

    // 使用 UserBufferReader 读取用户提供的缓冲区长度
    let optlen_reader = UserBufferReader::new(optlen, core::mem::size_of::<u32>(), from_user)?;
    let user_len = optlen_reader.buffer_protected(0)?.read_one::<u32>(0)? as usize;

    let get_filter = level == PSOL::SOCKET as usize
        && matches!(PSO::try_from(optname as u32), Ok(PSO::ATTACH_FILTER));
    if user_len > MAX_OPTVAL_LEN && !get_filter {
        return Err(SystemError::EINVAL);
    }

    // 获取socket
    let socket_inode = ProcessManager::current_pcb().get_socket_inode(fd as i32)?;
    let socket = socket_inode.as_socket().unwrap();

    let level = PSOL::try_from(level as u32)?;

    if matches!(level, PSOL::SOCKET) {
        let opt = PSO::try_from(optname as u32).map_err(|_| SystemError::ENOPROTOOPT)?;

        match opt {
            // SO_GET_FILTER shares value 26 with SO_ATTACH_FILTER. Unlike
            // ordinary getsockopt options, optlen is an instruction count.
            PSO::ATTACH_FILTER => {
                let insn_size = core::mem::size_of::<SockFilter>();
                let capacity = user_len.min(BPF_MAXINSNS);
                let mut kbuf = vec![0u8; capacity * insn_size];
                let count = socket.option(level, optname, &mut kbuf)?;

                if user_len != 0 && count != 0 {
                    let bytes = count * insn_size;
                    let mut optval_writer = UserBufferWriter::new(optval, bytes, from_user)?;
                    optval_writer.copy_to_user_protected(&kbuf[..bytes], 0)?;
                }

                let mut optlen_writer =
                    UserBufferWriter::new(optlen, core::mem::size_of::<u32>(), from_user)?;
                optlen_writer
                    .buffer_protected(0)?
                    .write_one::<u32>(0, &(count as u32))?;
                return Ok(0);
            }
            PSO::SNDBUF => {
                let need = core::mem::size_of::<u32>();
                let out_len = calc_out_len(optval, user_len, need);

                if !optval.is_null() && out_len != 0 {
                    let value = socket.send_buffer_size() as u32;
                    let bytes = value.to_ne_bytes();
                    let mut optval_writer = UserBufferWriter::new(optval, out_len, from_user)?;
                    optval_writer.copy_to_user_protected(&bytes[..out_len], 0)?;
                }

                // 写回选项值的实际长度（Linux ABI），而不是截断后的拷贝长度。
                let mut optlen_writer =
                    UserBufferWriter::new(optlen, core::mem::size_of::<u32>(), from_user)?;
                optlen_writer
                    .buffer_protected(0)?
                    .write_one::<u32>(0, &(need as u32))?;
                return Ok(0);
            }
            PSO::RCVBUF => {
                let need = core::mem::size_of::<u32>();
                let out_len = calc_out_len(optval, user_len, need);

                if !optval.is_null() && out_len != 0 {
                    let value = socket.recv_buffer_size() as u32;
                    let bytes = value.to_ne_bytes();
                    let mut optval_writer = UserBufferWriter::new(optval, out_len, from_user)?;
                    optval_writer.copy_to_user_protected(&bytes[..out_len], 0)?;
                }

                // 写回选项值的实际长度（Linux ABI），而不是截断后的拷贝长度。
                let mut optlen_writer =
                    UserBufferWriter::new(optlen, core::mem::size_of::<u32>(), from_user)?;
                optlen_writer
                    .buffer_protected(0)?
                    .write_one::<u32>(0, &(need as u32))?;
                return Ok(0);
            }
            _ => {
                // 其它 SOL_SOCKET 选项交给具体 socket 实现。
                // 这里采用"内核缓冲区 -> copy_to_user"的方式，避免假设 optval 是 u32。
                let kbuf_len = user_len.min(MAX_OPTVAL_LEN);
                let mut kbuf = vec![0u8; kbuf_len];
                let written = socket.option(level, optname, &mut kbuf)?;
                let out_len = calc_out_len(optval, user_len, written);

                if !optval.is_null() && out_len != 0 {
                    let mut optval_writer = UserBufferWriter::new(optval, out_len, from_user)?;
                    optval_writer.copy_to_user_protected(&kbuf[..out_len], 0)?;
                }

                // 写回选项值的实际长度（Linux ABI），而不是截断后的拷贝长度。
                let mut optlen_writer =
                    UserBufferWriter::new(optlen, core::mem::size_of::<u32>(), from_user)?;
                optlen_writer
                    .buffer_protected(0)?
                    .write_one::<u32>(0, &(written as u32))?;
                return Ok(0);
            }
        }
    }

    // To manipulate options at any other level the
    // protocol number of the appropriate protocol controlling the
    // option is supplied.  For example, to indicate that an option is
    // to be interpreted by the TCP protocol, level should be set to the
    // protocol number of TCP.

    if matches!(level, PSOL::TCP) {
        let _optname = TcpOption::try_from(optname as i32).map_err(|_| SystemError::ENOPROTOOPT)?;
        // TcpOption::Congestion => return Ok(0),
        // Other TCP options are delegated to the socket implementation below.
    }

    if matches!(level, PSOL::NETLINK) && optname == NETLINK_LIST_MEMBERSHIPS {
        let mut kbuf = vec![0u8; MAX_OPTVAL_LEN];
        let written = socket.option(level, optname, &mut kbuf)?;
        let out_len = if optval.is_null() {
            0
        } else {
            core::cmp::min(
                (user_len / core::mem::size_of::<u32>()) * core::mem::size_of::<u32>(),
                written,
            )
        };

        if !optval.is_null() && out_len != 0 {
            let mut optval_writer = UserBufferWriter::new(optval, out_len, from_user)?;
            optval_writer.copy_to_user_protected(&kbuf[..out_len], 0)?;
        }

        let mut optlen_writer =
            UserBufferWriter::new(optlen, core::mem::size_of::<u32>(), from_user)?;
        optlen_writer
            .buffer_protected(0)?
            .write_one::<u32>(0, &(written as u32))?;
        return Ok(0);
    }

    // 其它 level（如 SOL_IP/SOL_IPV6/SOL_RAW 等）交给具体 socket 实现。
    // gVisor raw_socket_test: getsockopt(SOL_IPV6, IPV6_CHECKSUM) 等
    {
        let kbuf_len = user_len.min(MAX_OPTVAL_LEN);
        let mut kbuf = vec![0u8; kbuf_len];
        let written = socket.option(level, optname, &mut kbuf)?;
        let out_len = calc_out_len(optval, user_len, written);

        if !optval.is_null() && out_len != 0 {
            let mut optval_writer = UserBufferWriter::new(optval, out_len, from_user)?;
            optval_writer.copy_to_user_protected(&kbuf[..out_len], 0)?;
        }

        // Linux 语义：*optlen 回写实际输出长度；optval != NULL 时为 min(user_len, written)，
        // optval == NULL 时为 written（用于探测选项大小）。
        let mut optlen_writer =
            UserBufferWriter::new(optlen, core::mem::size_of::<u32>(), from_user)?;
        optlen_writer
            .buffer_protected(0)?
            .write_one::<u32>(0, &(written as u32))?;
        Ok(0)
    }
}
