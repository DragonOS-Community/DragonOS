use system_error::SystemError;

use crate::arch::syscall::nr::SYS_WRITE;
use crate::process::ProcessManager;
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;
use crate::syscall::user_access::UserBufferReader;

use alloc::string::ToString;
use alloc::vec::Vec;

/// System call handler for the `write` syscall
///
/// This handler implements the `Syscall` trait to provide functionality for writing data to a file descriptor.
pub struct SysWriteHandle;

impl Syscall for SysWriteHandle {
    /// Returns the number of arguments expected by the `write` syscall
    fn num_args(&self) -> usize {
        3
    }

    /// Handles the `write` system call
    ///
    /// Writes data from a user buffer to the specified file descriptor.
    ///
    /// # Arguments
    /// * `args` - Array containing:
    ///   - args[0]: File descriptor (i32)
    ///   - args[1]: Pointer to user buffer (*const u8)
    ///   - args[2]: Length of data to write (usize)
    /// * `from_user` - Indicates if the call originates from user space
    ///
    /// # Returns
    /// * `Ok(usize)` - Number of bytes successfully written
    /// * `Err(SystemError)` - Error code if operation fails
    fn handle(&self, args: &[usize], from_user: bool) -> Result<usize, SystemError> {
        let fd = Self::fd(args);
        let buf_vaddr = Self::buf(args);
        let len = Self::len(args);

        let user_buffer_reader = UserBufferReader::new(buf_vaddr, len, from_user)?;

        let user_buf = user_buffer_reader.read_from_user(0)?;

        do_write(fd, user_buf)
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
        ]
    }
}

impl SysWriteHandle {
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
}

syscall_table_macros::declare_syscall!(SYS_WRITE, SysWriteHandle);

/// Internal implementation of the write operation
///
/// # Arguments
/// * `fd` - File descriptor to write to
/// * `buf` - Buffer containing data to write
///
/// # Returns
/// * `Ok(usize)` - Number of bytes successfully written
/// * `Err(SystemError)` - Error code if operation fails
pub(super) fn do_write(fd: i32, buf: &[u8]) -> Result<usize, SystemError> {
    let binding = ProcessManager::current_pcb().fd_table();
    let fd_table_guard = binding.read();

    let file = fd_table_guard
        .get_file_by_fd(fd)
        .ok_or(SystemError::EBADF)?;

    // drop guard 以避免无法调度的问题
    drop(fd_table_guard);
    return file.write(buf.len(), buf);
}
