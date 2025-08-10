//! System call handler for getting directory entries.

use system_error::SystemError;

use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::{SYS_GETDENTS, SYS_GETDENTS64};
use crate::filesystem::vfs::FilldirContext;
use crate::filesystem::vfs::file::FileDescriptorVec;
use crate::mm::{VirtAddr, verify_area};
use crate::process::ProcessManager;
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;
use alloc::string::ToString;
use alloc::vec::Vec;

/// System call handler for the `getdents` syscall
///
/// Reads directory entries from a directory file descriptor.
pub struct SysGetdentsHandle;

impl Syscall for SysGetdentsHandle {
    /// Returns the number of arguments expected by the `getdents` syscall
    fn num_args(&self) -> usize {
        3
    }

    /// # 获取目录中的数据
    ///
    /// ## 参数
    /// - fd 文件描述符号
    /// - buf 输出缓冲区
    ///
    /// ## 返回值
    /// - Ok(ctx.current_pos) 填充缓冲区当前指针位置
    /// - Err(ctx.error.unwrap()) 填充缓冲区时返回的错误
    fn handle(&self, args: &[usize], frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let fd = Self::fd(args);
        let buf_vaddr = Self::buf(args);
        let len = Self::len(args);
        let virt_addr: VirtAddr = VirtAddr::new(buf_vaddr);
        // 判断缓冲区是否来自用户态，进行权限校验
        let res = if frame.is_from_user() && verify_area(virt_addr, len).is_err() {
            // 来自用户态，而buffer在内核态，这样的操作不被允许
            Err(SystemError::EPERM)
        } else if buf_vaddr == 0 {
            Err(SystemError::EFAULT)
        } else {
            let buf: &mut [u8] = unsafe {
                core::slice::from_raw_parts_mut::<'static, u8>(buf_vaddr as *mut u8, len)
            };
            if fd < 0 || fd as usize > FileDescriptorVec::PROCESS_MAX_FD {
                return Err(SystemError::EBADF);
            }

            // 获取fd
            let binding = ProcessManager::current_pcb().fd_table();
            let fd_table_guard = binding.read();
            let file = fd_table_guard
                .get_file_by_fd(fd)
                .ok_or(SystemError::EBADF)?;

            // drop guard 以避免无法调度的问题
            drop(fd_table_guard);

            let mut ctx = FilldirContext::new(buf);
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
        };
        return res;
    }

    /// Formats the syscall parameters for display/debug purposes
    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("fd", Self::fd(args).to_string()),
            FormattedSyscallParam::new("buf", format!("{:#x}", Self::buf(args))),
            FormattedSyscallParam::new("count", Self::len(args).to_string()),
        ]
    }
}

impl SysGetdentsHandle {
    /// Extracts the file descriptor from syscall parameters
    fn fd(args: &[usize]) -> i32 {
        args[0] as i32
    }

    /// Extracts the buffer pointer from syscall parameters
    fn buf(args: &[usize]) -> usize {
        args[1]
    }

    /// Extracts the buffer size from syscall parameters
    fn len(args: &[usize]) -> usize {
        args[2]
    }
}

syscall_table_macros::declare_syscall!(SYS_GETDENTS, SysGetdentsHandle);
syscall_table_macros::declare_syscall!(SYS_GETDENTS64, SysGetdentsHandle);
