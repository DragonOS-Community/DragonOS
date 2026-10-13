//! System call handler for getting current working directory.

use system_error::SystemError;

use crate::alloc::string::ToString;
use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_GETCWD;
use crate::process::ProcessManager;
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;
use crate::syscall::user_access::UserBufferWriter;
use alloc::vec::Vec;

/// System call handler for the `getcwd` syscall
///
/// Gets the current working directory path and copies it to user buffer.
pub struct SysGetcwdHandle;

impl Syscall for SysGetcwdHandle {
    /// Returns the number of arguments expected by the `getcwd` syscall
    fn num_args(&self) -> usize {
        2
    }

    /// @brief 获取当前进程的工作目录路径
    ///
    /// @param buf 指向缓冲区的指针
    /// @param size 缓冲区的大小
    ///
    /// @return 成功，返回的指针指向包含工作目录路径的字符串
    /// @return 错误，没有足够的空间
    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let buf_vaddr = Self::buf(args);
        let size = Self::size(args);

        let mut cwd = ProcessManager::current_pcb().fs_struct().cwd_path()?;
        let length = cwd.len().checked_add(1).ok_or(SystemError::ENAMETOOLONG)?;
        if length > crate::filesystem::vfs::MAX_PATHLEN {
            return Err(SystemError::ENAMETOOLONG);
        }
        if length > size {
            return Err(SystemError::ERANGE);
        }
        cwd.try_reserve(1).map_err(|_| SystemError::ENOMEM)?;
        cwd.push('\0');
        // Path ownership/topology locks have all been released. Only actual
        // output bytes are checked/copied, not a user's arbitrary size range.
        let mut writer = UserBufferWriter::new(buf_vaddr, length, true)?;
        writer.copy_to_user_protected(cwd.as_bytes(), 0)?;
        Ok(length)
    }

    /// Formats the syscall parameters for display/debug purposes
    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("buf", format!("{:#x}", Self::buf(args) as usize)),
            FormattedSyscallParam::new("size", Self::size(args).to_string()),
        ]
    }
}

impl SysGetcwdHandle {
    /// Extracts the buffer pointer from syscall parameters
    fn buf(args: &[usize]) -> *mut u8 {
        args[0] as *mut u8
    }

    /// Extracts the buffer size from syscall parameters
    fn size(args: &[usize]) -> usize {
        args[1]
    }
}

syscall_table_macros::declare_syscall!(SYS_GETCWD, SysGetcwdHandle);
