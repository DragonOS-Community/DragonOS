//! System call handler for writing data at a specific offset.

use system_error::SystemError;

use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_PWRITE64;
use crate::process::ProcessManager;
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;
use alloc::string::ToString;
use alloc::vec::Vec;

use super::pread_pwrite_common::{do_pread_pwrite_at, PreadPwriteDir};

/// System call handler for the `pwrite64` syscall
///
/// Writes data to a file at a specific offset without changing the file position.
pub struct SysPwrite64Handle;

/// 校验 pwrite/pwritev 的偏移和长度是否符合 Linux 语义。
/// - 负偏移返回 EINVAL。
/// - 偏移、长度或偏移+长度超过 i64::MAX 也返回 EINVAL。
pub(super) fn validate_pwrite_range(offset: i64, len: usize) -> Result<usize, SystemError> {
    if offset < 0 {
        return Err(SystemError::EINVAL);
    }
    let offset_u64 = offset as u64;
    let len_u64 = len as u64;
    let max_off = i64::MAX as u64;
    let end = offset_u64.checked_add(len_u64).ok_or(SystemError::EINVAL)?;
    if offset_u64 > max_off || len_u64 > max_off || end > max_off {
        return Err(SystemError::EINVAL);
    }
    Ok(offset_u64 as usize)
}

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

        let binding = ProcessManager::current_pcb().fd_table();
        let fd_table_guard = binding.read();

        let file = fd_table_guard.get_file_by_fd(fd);
        if file.is_none() {
            return Err(SystemError::EBADF);
        }
        // drop guard 以避免无法调度的问题
        drop(fd_table_guard);
        let file = file.unwrap();

        // Linux/POSIX: count==0 must not touch the user buffer, but must still validate fd and flags.
        let offset = validate_pwrite_range(offset, len)?;
        do_pread_pwrite_at(
            file.as_ref(),
            offset,
            buf_vaddr as usize,
            len,
            frame.is_from_user(),
            PreadPwriteDir::Write,
        )
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
    fn offset(args: &[usize]) -> i64 {
        args[3] as i64
    }
}

syscall_table_macros::declare_syscall!(SYS_PWRITE64, SysPwrite64Handle);
