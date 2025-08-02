//! System call handler for seeking to a position in a file.

use system_error::SystemError;

use super::SEEK_MAX;
use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_LSEEK;
use crate::driver::base::block::SeekFrom;
use crate::process::ProcessManager;
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use super::{SEEK_CUR, SEEK_END, SEEK_SET};

/// System call handler for the `lseek` syscall
///
/// This handler implements the `Syscall` trait to provide functionality for seeking
/// to a specific position in a file.
pub struct SysLseekHandle;

impl Syscall for SysLseekHandle {
    /// Returns the number of arguments expected by the `lseek` syscall
    fn num_args(&self) -> usize {
        3
    }

    /// @brief 调整文件操作指针的位置
    ///
    /// @param fd 文件描述符编号
    /// @param seek 调整的方式
    ///
    /// @return Ok(usize) 调整后，文件访问指针相对于文件头部的偏移量
    /// @return Err(SystemError) 调整失败，返回posix错误码
    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let fd = Self::fd(args);
        let offset = Self::offset(args);
        let whence = Self::whence(args);

        // Convert seek type to SeekFrom enum
        let seek = match whence {
            SEEK_SET => Ok(SeekFrom::SeekSet(offset)),
            SEEK_CUR => Ok(SeekFrom::SeekCurrent(offset)),
            SEEK_END => Ok(SeekFrom::SeekEnd(offset)),
            SEEK_MAX => Ok(SeekFrom::SeekEnd(0)),
            _ => Err(SystemError::EINVAL),
        }?;

        // Get file from file descriptor table
        let binding = ProcessManager::current_pcb().fd_table();
        let fd_table_guard = binding.read();
        let file = fd_table_guard
            .get_file_by_fd(fd)
            .ok_or(SystemError::EBADF)?;

        // Drop guard to avoid scheduling issues
        drop(fd_table_guard);

        // Perform the seek operation
        return file.lseek(seek);
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
            FormattedSyscallParam::new("offset", Self::offset(args).to_string()),
            FormattedSyscallParam::new("whence", Self::whence_name(Self::whence(args))),
        ]
    }
}

impl SysLseekHandle {
    /// Extracts the file descriptor from syscall arguments
    ///
    /// # Arguments
    /// * `args` - The syscall arguments array
    ///
    /// # Returns
    /// The file descriptor as i32
    fn fd(args: &[usize]) -> i32 {
        args[0] as i32
    }

    /// Extracts the offset value from syscall arguments
    ///
    /// # Arguments
    /// * `args` - The syscall arguments array
    ///
    /// # Returns
    /// The offset value as i64
    fn offset(args: &[usize]) -> i64 {
        args[1] as i64
    }

    /// Extracts the whence parameter from syscall arguments
    ///
    /// # Arguments
    /// * `args` - The syscall arguments array
    ///
    /// # Returns
    /// The whence parameter as u32
    fn whence(args: &[usize]) -> u32 {
        args[2] as u32
    }

    /// Converts whence value to human-readable name for debugging
    ///
    /// # Arguments
    /// * `whence` - The whence parameter value
    ///
    /// # Returns
    /// String representation of the whence parameter
    fn whence_name(whence: u32) -> String {
        match whence {
            SEEK_SET => "SEEK_SET".to_string(),
            SEEK_CUR => "SEEK_CUR".to_string(),
            SEEK_END => "SEEK_END".to_string(),
            SEEK_MAX => "SEEK_MAX".to_string(),
            _ => format!("UNKNOWN({})", whence),
        }
    }
}

syscall_table_macros::declare_syscall!(SYS_LSEEK, SysLseekHandle);
