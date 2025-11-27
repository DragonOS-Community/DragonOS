//! System call handler for getting directory entries.

use system_error::SystemError;

use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::{SYS_GETDENTS, SYS_GETDENTS64};
use crate::filesystem::vfs::{DirentFormat, FilldirContext};
use crate::process::ProcessManager;
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;
use crate::syscall::user_access::UserBufferWriter;
use alloc::string::ToString;
use alloc::vec::Vec;

/// System call handler for the `getdents` syscall (旧格式)
///
/// Reads directory entries from a directory file descriptor using linux_dirent format.
pub struct SysGetdentsHandle;

/// System call handler for the `getdents64` syscall (新格式)
///
/// Reads directory entries from a directory file descriptor using linux_dirent64 format.
pub struct SysGetdents64Handle;

const MAX_GETDENTS_COUNT: usize = i32::MAX as usize;

fn do_getdents(
    args: &[usize],
    frame: &mut TrapFrame,
    format: DirentFormat,
) -> Result<usize, SystemError> {
    let fd = args[0] as i32;
    let buf_vaddr = args[1];
    let len = args[2];

    if buf_vaddr == 0 {
        return Err(SystemError::EFAULT);
    }

    if len == 0 {
        return Err(SystemError::EINVAL);
    }

    if len > MAX_GETDENTS_COUNT {
        return Err(SystemError::EINVAL);
    }

    if fd < 0 {
        return Err(SystemError::EBADF);
    }

    // 使用 UserBufferWriter 和 buffer_protected 创建受保护的用户缓冲区
    let from_user = frame.is_from_user();
    let mut writer = UserBufferWriter::new(buf_vaddr as *mut u8, len, from_user)?;
    let user_buf = writer.buffer_protected(0)?;

    // 获取fd
    let binding = ProcessManager::current_pcb().fd_table();
    let fd_table_guard = binding.read();
    let file = fd_table_guard
        .get_file_by_fd(fd)
        .ok_or(SystemError::EBADF)?;

    // drop guard 以避免无法调度的问题
    drop(fd_table_guard);

    let mut ctx = FilldirContext::new(user_buf, format);
    match file.read_dir(&mut ctx) {
        Ok(_) => {
            if ctx.error.is_some() {
                if ctx.error == Some(SystemError::EINVAL) {
                    return Ok(ctx.current_pos);
                } else {
                    return Err(ctx.error.unwrap());
                }
            }
            return Ok(ctx.current_pos);
        }
        Err(e) => {
            return Err(e);
        }
    }
}

impl Syscall for SysGetdentsHandle {
    /// Returns the number of arguments expected by the `getdents` syscall
    fn num_args(&self) -> usize {
        3
    }

    /// # 获取目录中的数据 (旧格式 linux_dirent)
    ///
    /// ## 参数
    /// - fd 文件描述符号
    /// - buf 输出缓冲区
    ///
    /// ## 返回值
    /// - Ok(ctx.current_pos) 填充缓冲区当前指针位置
    /// - Err(ctx.error.unwrap()) 填充缓冲区时返回的错误
    fn handle(&self, args: &[usize], frame: &mut TrapFrame) -> Result<usize, SystemError> {
        do_getdents(args, frame, DirentFormat::Getdents)
    }

    /// Formats the syscall parameters for display/debug purposes
    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("fd", args[0].to_string()),
            FormattedSyscallParam::new("buf", format!("{:#x}", args[1])),
            FormattedSyscallParam::new("count", args[2].to_string()),
        ]
    }
}

impl Syscall for SysGetdents64Handle {
    /// Returns the number of arguments expected by the `getdents64` syscall
    fn num_args(&self) -> usize {
        3
    }

    /// # 获取目录中的数据 (新格式 linux_dirent64)
    ///
    /// ## 参数
    /// - fd 文件描述符号
    /// - buf 输出缓冲区
    ///
    /// ## 返回值
    /// - Ok(ctx.current_pos) 填充缓冲区当前指针位置
    /// - Err(ctx.error.unwrap()) 填充缓冲区时返回的错误
    fn handle(&self, args: &[usize], frame: &mut TrapFrame) -> Result<usize, SystemError> {
        do_getdents(args, frame, DirentFormat::Getdents64)
    }

    /// Formats the syscall parameters for display/debug purposes
    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("fd", args[0].to_string()),
            FormattedSyscallParam::new("buf", format!("{:#x}", args[1])),
            FormattedSyscallParam::new("count", args[2].to_string()),
        ]
    }
}

syscall_table_macros::declare_syscall!(SYS_GETDENTS, SysGetdentsHandle);
syscall_table_macros::declare_syscall!(SYS_GETDENTS64, SysGetdents64Handle);
