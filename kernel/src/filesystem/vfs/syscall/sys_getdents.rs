//! System call handler for getting directory entries.

use system_error::SystemError;

use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::{SYS_GETDENTS, SYS_GETDENTS64};
use crate::filesystem::vfs::{DirentFormat, FilldirContext};
use crate::mm::VirtAddr;
use crate::process::ProcessManager;
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;
use crate::syscall::user_buffer::UserBuffer;
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
    _frame: &mut TrapFrame,
    format: DirentFormat,
) -> Result<usize, SystemError> {
    let fd = args[0] as i32;
    let buf_vaddr = args[1];
    let len = args[2];

    // Linux 语义：count 的合法性优先于 buf 指针可访问性检查。
    // 例如 count 太小应返回 EINVAL（即使 buf 指向坏地址）。
    if len == 0 {
        return Err(SystemError::EINVAL);
    }

    if len > MAX_GETDENTS_COUNT {
        return Err(SystemError::EINVAL);
    }

    // 最小可写入的 dirent 头大小（不含 d_name）
    let min_dirent_size = match format {
        // offsetof(struct linux_dirent, d_name)
        DirentFormat::Getdents => 18,
        // offsetof(struct linux_dirent64, d_name)
        DirentFormat::Getdents64 => 19,
    };
    if len < min_dirent_size {
        return Err(SystemError::EINVAL);
    }

    if buf_vaddr == 0 {
        return Err(SystemError::EFAULT);
    }

    if fd < 0 {
        return Err(SystemError::EBADF);
    }

    // 注意：这里不能对整个 [buf, buf+count) 做 verify_area，否则无法实现
    // “部分可访问时先写入一部分再因 EFAULT 停止并返回已写入字节数”的 Linux 行为。
    // UserBuffer 的实际读写都通过异常表保护的 copy_*_user_protected 完成。
    let user_buf: UserBuffer<'static> = unsafe { UserBuffer::new(VirtAddr::new(buf_vaddr), len) };

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
            if let Some(err) = ctx.error {
                // Linux 语义：如果已经写入了部分 dirent，则返回已写入字节数；
                // 只有在“一个字节都没写入”的情况下才返回具体错误码。
                if ctx.current_pos > 0 {
                    return Ok(ctx.current_pos);
                }
                return Err(err);
            }
            Ok(ctx.current_pos)
        }
        Err(e) => {
            // 很多文件系统会把 filldir 的返回值（例如 -EFAULT）直接向上层传播。
            // 但 Linux getdents* 的语义是：只要已经写入了至少一个条目，就返回已写入的字节数。
            if ctx.current_pos > 0 {
                return Ok(ctx.current_pos);
            }
            Err(e)
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
