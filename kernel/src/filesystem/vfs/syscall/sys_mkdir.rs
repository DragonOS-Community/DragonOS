use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_MKDIR;
use crate::filesystem::vfs::fcntl::AtFlags;
use crate::filesystem::vfs::vcore::do_mkdir_at;
use crate::filesystem::vfs::InodeMode;
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;
use crate::syscall::user_access::vfs_check_and_clone_cstr;
use alloc::vec::Vec;
use system_error::SystemError;

pub struct SysMkdirHandle;

impl Syscall for SysMkdirHandle {
    /// Returns the number of arguments this syscall takes.
    fn num_args(&self) -> usize {
        2
    }

    /// @brief 创建文件夹
    ///
    /// @param path(r8) 路径 / mode(r9) 模式
    ///
    /// @return uint64_t 负数错误码 / 0表示成功
    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let path = Self::path(args);
        let mode = Self::mode(args);

        let path = vfs_check_and_clone_cstr(path, Some(crate::filesystem::vfs::MAX_PATHLEN))?
            .into_string()
            .map_err(|_| SystemError::EINVAL)?;

        do_mkdir_at(
            AtFlags::AT_FDCWD.bits(),
            &path,
            InodeMode::from_bits_truncate(mode as u32),
        )?;
        return Ok(0);
    }

    /// Formats the syscall arguments for display/debugging purposes.
    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("path", format!("{:#x}", Self::path(args) as usize)),
            FormattedSyscallParam::new("mode", format!("{:#x}", Self::mode(args))),
        ]
    }
}

impl SysMkdirHandle {
    /// Extracts the path argument from syscall parameters.
    fn path(args: &[usize]) -> *const u8 {
        args[0] as *const u8
    }

    /// Extracts the mode argument from syscall parameters.
    fn mode(args: &[usize]) -> usize {
        args[1]
    }
}

syscall_table_macros::declare_syscall!(SYS_MKDIR, SysMkdirHandle);
