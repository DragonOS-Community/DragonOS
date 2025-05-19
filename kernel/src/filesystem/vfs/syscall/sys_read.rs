use system_error::SystemError;

use crate::arch::syscall::nr::SYS_READ;
use crate::process::ProcessManager;
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;
use crate::syscall::user_access::UserBufferWriter;

use alloc::string::ToString;
use alloc::vec::Vec;

/// System call handler for the `read` syscall
///
/// This handler implements the `Syscall` trait to provide functionality for reading data from a file descriptor.
pub struct SysReadHandle;

impl Syscall for SysReadHandle {
    /// Returns the number of arguments expected by the `read` syscall
    fn num_args(&self) -> usize {
        3
    }

    /// Handles the `read` system call
    ///
    /// Reads data from the specified file descriptor into a user buffer.
    ///
    /// # Arguments
    /// * `args` - Array containing:
    ///   - args[0]: File descriptor (i32)
    ///   - args[1]: Pointer to user buffer (*mut u8)
    ///   - args[2]: Length of data to read (usize)
    /// * `from_user` - Indicates if the call originates from user space
    ///
    /// # Returns
    /// * `Ok(usize)` - Number of bytes successfully read
    /// * `Err(SystemError)` - Error code if operation fails
    fn handle(&self, args: &[usize], from_user: bool) -> Result<usize, SystemError> {
        let fd = Self::fd(args);
        let buf_vaddr = Self::buf(args);
        let len = Self::len(args);

        let mut user_buffer_writer = UserBufferWriter::new(buf_vaddr, len, from_user)?;

        let user_buf = user_buffer_writer.buffer(0)?;
        do_read(fd, user_buf)
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

impl SysReadHandle {
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
}

syscall_table_macros::declare_syscall!(SYS_READ, SysReadHandle);

/// Internal implementation of the read operation
///
/// # Arguments
/// * `fd` - File descriptor to read from
/// * `buf` - Buffer to store read data
///
/// # Returns
/// * `Ok(usize)` - Number of bytes successfully read
/// * `Err(SystemError)` - Error code if operation fails
pub(super) fn do_read(fd: i32, buf: &mut [u8]) -> Result<usize, SystemError> {
    let binding = ProcessManager::current_pcb().fd_table();
    let fd_table_guard = binding.read();

    let file = fd_table_guard.get_file_by_fd(fd);
    if file.is_none() {
        return Err(SystemError::EBADF);
    }
    // drop guard 以避免无法调度的问题
    drop(fd_table_guard);
    let file = file.unwrap();

    return file.read(buf.len(), buf);
}
