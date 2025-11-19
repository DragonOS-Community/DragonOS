use system_error::SystemError;

use crate::arch::syscall::nr::SYS_WRITEV;
use crate::filesystem::vfs::iov::IoVec;
use crate::filesystem::vfs::iov::IoVecs;
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;

use alloc::string::ToString;
use alloc::vec::Vec;

use super::sys_write::do_write;
use crate::arch::interrupt::TrapFrame;
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
    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let fd = Self::fd(args);
        let iov = Self::iov(args);
        let count = Self::count(args);

        
        // 将用户态传入的数据结构 `IoVecs` 重新在内核上构造
        let iovecs = unsafe { IoVecs::from_user(iov, count, false) }?;
        let data = iovecs.gather()?;
        
        // TODO: 支持零内核拷贝的分散写 （需要文件系统底层支持分散写）
        // - 直接将传入的用户态 IoVec 使用 vma 做校验以后传入底层文件系统进行分散写，避免内核拷贝
        // - 实现路径（linux）：wirtev --> vfs_writev --> do_iter_write --> do_loop_readv_writev/do_iter_readv_writev
        // - 目前内核文件子系统尚未实现分散写功能，即无法直接使用用户态的 IoVec 进行写操作
        // - 目前先将用户态的 IoVec 聚合成一个连续的内核缓冲区 `data`，然后进行写操作，避免多次发起写操作的开销。
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
