use system_error::SystemError;

use crate::arch::syscall::nr::SYS_WRITEV;
use crate::filesystem::vfs::iov::IoVec;
use crate::filesystem::vfs::iov::IoVecs;
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;

use alloc::string::ToString;
use alloc::vec::Vec;

use super::sys_write::do_write;

/// System call handler for `writev` operation
///
/// The `writev` system call writes data from multiple buffers to a file descriptor.
/// It is equivalent to multiple `write` calls but is more efficient.
pub struct SysWriteVHandle;

impl Syscall for SysWriteVHandle {
    /// Returns the number of arguments required by the `writev` system call
    fn num_args(&self) -> usize {
        3
    }

    /// Handles the `writev` system call
    ///
    /// # Arguments
    /// * `args` - System call arguments containing:
    ///   * `fd`: File descriptor to write to
    ///   * `iov`: Pointer to array of I/O vectors
    ///   * `count`: Number of elements in the I/O vector array
    /// * `_from_user` - Flag indicating if the call originated from user space
    ///
    /// # Returns
    /// * `Ok(usize)` - Number of bytes written
    /// * `Err(SystemError)` - Error that occurred during operation
    ///
    /// # Safety
    /// The caller must ensure the `iov` pointer is valid and points to properly initialized memory.
    fn handle(&self, args: &[usize], _from_user: bool) -> Result<usize, SystemError> {
        let fd = Self::fd(args);
        let iov = Self::iov(args);
        let count = Self::count(args);

        // IoVecs会进行用户态检验
        let iovecs = unsafe { IoVecs::from_user(iov, count, false) }?;
        let data = iovecs.gather();
        do_write(fd, &data)
    }

    /// Formats the system call parameters for display/debug purposes
    ///
    /// # Arguments
    /// * `args` - System call arguments to format
    ///
    /// # Returns
    /// Vector of formatted parameters with their names and values
    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("fd", Self::fd(args).to_string()),
            FormattedSyscallParam::new("iov", format!("{:#x}", Self::iov(args) as usize)),
            FormattedSyscallParam::new("count", Self::count(args).to_string()),
        ]
    }
}

impl SysWriteVHandle {
    /// Extracts the file descriptor from system call arguments
    fn fd(args: &[usize]) -> i32 {
        args[0] as i32
    }

    /// Extracts the I/O vector pointer from system call arguments
    fn iov(args: &[usize]) -> *const IoVec {
        args[1] as *const IoVec
    }

    /// Extracts the I/O vector count from system call arguments
    fn count(args: &[usize]) -> usize {
        args[2]
    }
}

syscall_table_macros::declare_syscall!(SYS_WRITEV, SysWriteVHandle);
