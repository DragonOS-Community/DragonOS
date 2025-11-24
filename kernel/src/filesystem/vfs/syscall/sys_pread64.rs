//! System call handler for reading data at a specific offset.

use system_error::SystemError;

use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_PREAD64;
use crate::filesystem::vfs::FileType;
use crate::process::ProcessManager;
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;
use crate::syscall::user_access::UserBufferWriter;
use alloc::string::ToString;
use alloc::vec::Vec;

/// System call handler for the `pread64` syscall
///
/// Reads data from a file at a specific offset without changing the file position.
pub struct SysPread64Handle;

impl Syscall for SysPread64Handle {
    /// Returns the number of arguments expected by the `pread64` syscall
    fn num_args(&self) -> usize {
        4
    }

    /// # sys_pread64 系统调用的实际执行函数
    ///
    /// ## 参数
    /// - `fd`: 文件描述符
    /// - `buf`: 读出缓冲区
    /// - `len`: 要读取的字节数
    /// - `offset`: 文件偏移量
    fn handle(&self, args: &[usize], frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let fd = Self::fd(args);
        let buf_vaddr = Self::buf(args);
        let len = Self::len(args);
        let offset = Self::offset(args);

        // 检查offset + len是否溢出 同时检查offset是否为负数

        let end_pos = offset.checked_add(len).ok_or(SystemError::EINVAL)?;
        if offset > i64::MAX as usize || end_pos > i64::MAX as usize {
            return Err(SystemError::EINVAL);
        }

        let mut user_buffer_writer =
            UserBufferWriter::new_checked(buf_vaddr, len, frame.is_from_user())?;
        let user_buf = user_buffer_writer.buffer(0)?;

        let binding = ProcessManager::current_pcb().fd_table();
        let fd_table_guard = binding.read();

        let file = fd_table_guard
            .get_file_by_fd(fd)
            .ok_or(SystemError::EBADF)?;

        // Drop guard to avoid scheduling issues
        drop(fd_table_guard);

        // 检查是否是管道/Socket (ESPIPE)
        let md = file.metadata()?;
        if md.file_type == FileType::Pipe
            || md.file_type == FileType::Socket
            || md.file_type == FileType::CharDevice
        {
            return Err(SystemError::ESPIPE);
        }

        return file.pread(offset, len, user_buf);
    }

    /// Formats the syscall parameters for display/debug purposes
    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("fd", Self::fd(args).to_string()),
            FormattedSyscallParam::new("buf", format!("{:#x}", Self::buf(args) as usize)),
            FormattedSyscallParam::new("count", Self::len(args).to_string()),
            FormattedSyscallParam::new("offset", Self::offset(args).to_string()),
        ]
    }
}

impl SysPread64Handle {
    /// Extracts the file descriptor from syscall arguments
    fn fd(args: &[usize]) -> i32 {
        args[0] as i32
    }

    /// Extracts the buffer pointer from syscall arguments
    fn buf(args: &[usize]) -> *mut u8 {
        args[1] as *mut u8
    }

    /// Extracts the buffer length from syscall arguments
    fn len(args: &[usize]) -> usize {
        args[2]
    }

    /// Extracts the file offset from syscall arguments
    fn offset(args: &[usize]) -> usize {
        args[3]
    }
}

syscall_table_macros::declare_syscall!(SYS_PREAD64, SysPread64Handle);
