use system_error::SystemError;

use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_GETSOCKOPT;
use crate::arch::MMArch;
use crate::mm::MemoryManagementArch;
use crate::net::socket;
use crate::process::ProcessManager;
use crate::syscall::table::{FormattedSyscallParam, Syscall};
use crate::syscall::user_access::{UserBufferReader, UserBufferWriter};
use alloc::string::ToString;
use alloc::vec::Vec;

/// getsockopt optval 最大长度限制（一页）
const MAX_OPTVAL_LEN: usize = MMArch::PAGE_SIZE;

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

    if user_len > MAX_OPTVAL_LEN {
        return Err(SystemError::EINVAL);
    }

    // 获取socket
    let socket_inode = ProcessManager::current_pcb().get_socket_inode(fd as i32)?;
    let socket = socket_inode.as_socket().unwrap();

    use socket::{PSO, PSOL};

    let level = PSOL::try_from(level as u32)?;

    if matches!(level, PSOL::SOCKET) {
        let opt = PSO::try_from(optname as u32).map_err(|_| SystemError::ENOPROTOOPT)?;

        match opt {
            PSO::SNDBUF => {
                let need = core::mem::size_of::<u32>();

                // 使用 UserBufferWriter 写入 optval（小数据）
                if !optval.is_null() {
                    let to_write = core::cmp::min(user_len, need);
                    let value = socket.send_buffer_size() as u32;
                    let mut optval_writer = UserBufferWriter::new(optval, to_write, from_user)?;
                    optval_writer
                        .buffer_protected(0)?
                        .write_one::<u32>(0, &value)?;
                }

                // 写回实际需要的长度
                let mut optlen_writer =
                    UserBufferWriter::new(optlen, core::mem::size_of::<u32>(), from_user)?;
                optlen_writer
                    .buffer_protected(0)?
                    .write_one::<u32>(0, &(need as u32))?;
                return Ok(0);
            }
            PSO::RCVBUF => {
                let need = core::mem::size_of::<u32>();
                let value = socket.recv_buffer_size() as u32;

                // 使用 UserBufferWriter 写入 optval（小数据）
                if !optval.is_null() {
                    let to_write = core::cmp::min(user_len, need);
                    let mut optval_writer = UserBufferWriter::new(optval, to_write, from_user)?;
                    optval_writer
                        .buffer_protected(0)?
                        .write_one::<u32>(0, &value)?;
                }

                // 写回实际需要的长度
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
                let mut kbuf = [0u8; 64];
                let written = socket.option(level, optname, &mut kbuf)?;
                let need = written;

                // 使用 UserBufferWriter 写入 optval（可能大数据）
                if !optval.is_null() {
                    let to_write = core::cmp::min(user_len, need);
                    let mut optval_writer = UserBufferWriter::new(optval, to_write, from_user)?;
                    optval_writer.copy_to_user_protected(&kbuf[..to_write], 0)?;
                }

                // 写回实际需要的长度
                let mut optlen_writer =
                    UserBufferWriter::new(optlen, core::mem::size_of::<u32>(), from_user)?;
                optlen_writer
                    .buffer_protected(0)?
                    .write_one::<u32>(0, &(need as u32))?;
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
        use socket::inet::stream::TcpOption;
        let optname = TcpOption::try_from(optname as i32).map_err(|_| SystemError::ENOPROTOOPT)?;
        match optname {
            TcpOption::Congestion => return Ok(0),
            _ => {
                return Err(SystemError::ENOPROTOOPT);
            }
        }
    }
    Err(SystemError::ENOPROTOOPT)
}
