use alloc::string::ToString;
use alloc::vec::Vec;

use system_error::SystemError;

use super::sys_write::get_file_by_fd;
use crate::arch::syscall::nr::SYS_PWRITEV;
use crate::filesystem::vfs::iov::{IoVec, IoVecs};
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

        if offset < 0 {
            return Err(SystemError::EINVAL);
        }
        let file = get_file_by_fd(fd)?;
        // Positional capability precedes importing even the iov metadata.
        // WRITE/CAN_WRITE are checked after import, inside the File entry.
        file.verify_pwrite_access()?;
        let iovecs = unsafe { IoVecs::from_user(iov, iov_count, false) }?;
        file.pwrite_user_vectored(offset as usize, &iovecs, false)
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
    fn offset(args: &[usize]) -> i64 {
        args[3] as i64
    }
}

syscall_table_macros::declare_syscall!(SYS_PWRITEV, SysPwriteVHandle);
