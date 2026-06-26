use crate::arch::syscall::nr::SYS_FALLOCATE;
use crate::{
    arch::interrupt::TrapFrame,
    process::ProcessManager,
    syscall::table::{FormattedSyscallParam, Syscall},
};
use alloc::vec::Vec;
use system_error::SystemError;

/// # fallocate
///
/// ## 描述
///
/// 为文件分配空间.
///
/// ## 参数
///
/// - `fd`：文件描述符
/// - `mode`：操作模式 (当前仅支持 0)
/// - `offset`：起始偏移量
/// - `len`：长度
///
/// ## 返回值
///
/// 如果成功，返回0，否则返回错误码.
///
/// ## 说明
///
/// fallocate 允许调用者直接操作文件分配的磁盘空间。
/// 默认操作 (mode=0) 会分配磁盘空间，如果 offset+len 大于文件大小，
/// 则会扩展文件大小。这与 posix_fallocate() 的行为类似。
///
/// 当前仅支持 mode=0 的默认操作，其他模式 (如 FALLOC_FL_KEEP_SIZE,
/// FALLOC_FL_PUNCH_HOLE 等) 暂不支持，会返回 EOPNOTSUPP_OR_ENOTSUP。
pub struct SysFallocateHandle;

impl Syscall for SysFallocateHandle {
    fn num_args(&self) -> usize {
        4
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let fd = Self::fd(args);
        let mode = Self::mode(args)?;
        let offset = Self::offset(args)?;
        let len = Self::len(args)?;

        let binding = ProcessManager::current_pcb().fd_table();
        let fd_table_guard = binding.read();

        if let Some(file) = fd_table_guard.get_file_by_fd(fd) {
            drop(fd_table_guard);

            return crate::filesystem::vfs::vcore::vfs_fallocate_file(file, mode, offset, len)
                .map(|_| 0);
        }

        return Err(SystemError::EBADF);
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("fd", format!("{:#x}", Self::fd(args))),
            FormattedSyscallParam::new("mode", format!("{:#x}", Self::mode(args).unwrap_or(0))),
            FormattedSyscallParam::new("offset", format!("{:#x}", Self::offset(args).unwrap_or(0))),
            FormattedSyscallParam::new("len", format!("{:#x}", Self::len(args).unwrap_or(0))),
        ]
    }
}

impl SysFallocateHandle {
    fn fd(args: &[usize]) -> i32 {
        args[0] as i32
    }

    fn mode(args: &[usize]) -> Result<i32, SystemError> {
        Ok(args[1] as i32)
    }

    fn offset(args: &[usize]) -> Result<usize, SystemError> {
        let offset = args[2];
        if offset > isize::MAX as usize {
            return Err(SystemError::EINVAL);
        }
        Ok(offset)
    }

    fn len(args: &[usize]) -> Result<usize, SystemError> {
        let len = args[3];
        if len > isize::MAX as usize {
            return Err(SystemError::EINVAL);
        }
        Ok(len)
    }
}

syscall_table_macros::declare_syscall!(SYS_FALLOCATE, SysFallocateHandle);
