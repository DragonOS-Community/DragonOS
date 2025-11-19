use alloc::string::ToString;
use alloc::vec::Vec;

use system_error::SystemError;

use crate::arch::syscall::nr::SYS_PWRITEV;
use crate::filesystem::vfs::iov::{IoVec, IoVecs};
use crate::process::ProcessManager;
use crate::syscall::table::{FormattedSyscallParam, Syscall};

pub struct SysPwriteVHandle;

impl Syscall for SysPwriteVHandle {
    fn num_args(&self) -> usize {
        4
    }

    /// ## Handle SYS_PWRITEV Call
    /// ### Arguments
    /// - `args` - System call arguments containing:
    ///   - `fd`: File descriptor to write to
    ///   - `iov`: Pointer to array of I/O vectors
    ///   - `iov_count`: Number of elements in the I/O vector array
    ///   - `offset`: at which the output operation is to be performed.
    /// - `frame`: Trap frame representing the current CPU register state and execution context of the calling process.
    ///   Used for accessing or modifying process state during syscall handling.
    /// ### Return
    /// - `Ok(usize)`: Number of bytes written
    /// - `Err(SystemError)`: Error that occurred during the operation  
    fn handle(
        &self,
        args: &[usize],
        _frame: &mut crate::arch::interrupt::TrapFrame,
    ) -> Result<usize, SystemError> {
        // 从 args buffer 中获取想要的参数
        let fd = Self::fd(args);
        let iov = Self::iov(args);
        let iov_count = Self::iov_count(args);
        let offset = Self::offset(args);

        // 将用户态传入的数据结构 `IoVecs` 重新在内核上构造
        let iovecs = unsafe { IoVecs::from_user(iov, iov_count, false) }?;
        let data = iovecs.gather()?;
        
        // TODO: 支持零内核拷贝的分散写 （需要文件系统底层支持分散写）
        // - 直接将传入的用户态 IoVec 使用 vma 做校验以后传入底层文件系统进行分散写，避免内核拷贝
        // - 实现路径（linux）：wirtev --> vfs_writev --> do_iter_write --> do_loop_readv_writev/do_iter_readv_writev
        // - 目前内核文件子系统尚未实现分散写功能，即无法直接使用用户态的 IoVec 进行写操作
        // - 目前先将用户态的 IoVec 聚合成一个连续的内核缓冲区 `data`，然后进行写操作，避免多次发起写操作的开销。
        do_pwritev(fd, &data, offset)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("fd:", Self::fd(args).to_string()),
            FormattedSyscallParam::new("iov:", format!("{:#x}", Self::iov(args) as usize)),
            FormattedSyscallParam::new("iov_count:", Self::iov_count(args).to_string()),
            FormattedSyscallParam::new("offset:", Self::offset(args).to_string()),
        ]
    }
}

impl SysPwriteVHandle {
    /// Extracts the file descriptor from system call arguments
    fn fd(args: &[usize]) -> i32 {
        args[0] as i32
    }

    /// Extracts the I/O vector pointer from system call arguments
    fn iov(args: &[usize]) -> *const IoVec {
        args[1] as *const IoVec
    }

    /// Extracts the I/O vector count from system call arguments
    fn iov_count(args: &[usize]) -> usize {
        args[2]
    }

    /// Extracts the offset at which the output operation is to be performed
    fn offset(args: &[usize]) -> usize {
        args[3]
    }
}

pub fn do_pwritev(fd: i32, buf: &[u8], offset: usize) -> Result<usize, SystemError> {
    let binding = ProcessManager::current_pcb().fd_table();
    let fd_table_guard = binding.read();

    let file = fd_table_guard
        .get_file_by_fd(fd)
        .ok_or(SystemError::EBADF)?;

    // 释放 fd_table_guard 的读锁
    drop(fd_table_guard);
    file.pwrite(offset, buf.len(), buf)
}

syscall_table_macros::declare_syscall!(SYS_PWRITEV, SysPwriteVHandle);
