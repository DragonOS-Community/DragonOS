//! System call handler for writing data at a specific offset.

use system_error::SystemError;

use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_PWRITE64;
use crate::arch::MMArch;
use crate::mm::{access_ok, MemoryManagementArch, VirtAddr};
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;
use crate::syscall::user_buffer::UserBuffer;
use alloc::string::ToString;
use alloc::vec::Vec;

use super::sys_write::get_file_by_fd;

/// System call handler for the `pwrite64` syscall
///
/// Writes data to a file at a specific offset without changing the file position.
pub struct SysPwrite64Handle;

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

        // Linux rejects a negative position before looking up the descriptor.
        if offset < 0 {
            return Err(SystemError::EINVAL);
        }
        let file = get_file_by_fd(fd)?;
        file.verify_pwrite_access()?;
        file.writeable()?;

        // Numerical access validation precedes the position-range check and
        // MAX_RW_COUNT clamp. Do not probe mappings or copy payload here:
        // the owner must enter its privilege/content transaction first.
        let address = VirtAddr::new(buf_vaddr as usize);
        if frame.is_from_user() {
            access_ok(address, len).map_err(|_| SystemError::EFAULT)?;
        }
        let offset = offset as usize;
        file.verify_pread_range(offset, len)?;
        let count = len.min((i32::MAX as usize) & !(MMArch::PAGE_SIZE - 1));
        if frame.is_from_user() {
            let input = unsafe { UserBuffer::new(address, len) };
            file.pwrite_user(offset, count, &input)
        } else {
            let buffer = if count == 0 {
                &[]
            } else {
                unsafe { core::slice::from_raw_parts(buf_vaddr, count) }
            };
            file.pwrite(offset, count, buffer)
        }
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
