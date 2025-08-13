//! System call handler for writing data at a specific offset.

use system_error::SystemError;

use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_PWRITE64;
use crate::process::ProcessManager;
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;
use crate::syscall::user_access::UserBufferReader;
use alloc::string::ToString;
use alloc::vec::Vec;

/// System call handler for the `pwrite64` syscall
///
/// Writes data to a file at a specific offset without changing the file position.
pub struct SysPwrite64Handle;

impl Syscall for SysPwrite64Handle {
    /// Returns the number of arguments expected by the `pwrite64` syscall
    fn num_args(&self) -> usize {
        4
    }

    /// # sys_pwrite64 系统调用的实际执行函数
    ///
    /// ## 参数
    /// - `fd`: 文件描述符
    /// - `buf`: 写入缓冲区
    /// - `len`: 要写入的字节数
    /// - `offset`: 文件偏移量
    fn handle(&self, args: &[usize], frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let fd = Self::fd(args);
        let buf_vaddr = Self::buf(args);
        let len = Self::len(args);
        let offset = Self::offset(args);

        let user_buffer_reader = UserBufferReader::new(buf_vaddr, len, frame.is_from_user())?;
        let user_buf = user_buffer_reader.read_from_user(0)?;

        let binding = ProcessManager::current_pcb().fd_table();
        let fd_table_guard = binding.read();

        let file = fd_table_guard.get_file_by_fd(fd);
        if file.is_none() {
            return Err(SystemError::EBADF);
        }
        // drop guard 以避免无法调度的问题
        drop(fd_table_guard);
        let file = file.unwrap();

        return file.pwrite(offset, len, user_buf);
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

impl SysPwrite64Handle {
    /// Extracts the file descriptor from syscall arguments
    fn fd(args: &[usize]) -> i32 {
        args[0] as i32
    }

    /// Extracts the buffer pointer from syscall arguments
    fn buf(args: &[usize]) -> *const u8 {
        args[1] as *const u8
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

syscall_table_macros::declare_syscall!(SYS_PWRITE64, SysPwrite64Handle);
