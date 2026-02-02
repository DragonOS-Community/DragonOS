use super::InodeMode;
use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_MKNODAT;
use crate::driver::base::device::device_number::DeviceNumber;
use crate::filesystem::vfs::syscall::AtFlags;
use crate::filesystem::vfs::utils::rsplit_path;
use crate::filesystem::vfs::utils::user_path_at;
use crate::filesystem::vfs::MAX_PATHLEN;
use crate::filesystem::vfs::VFS_MAX_FOLLOW_SYMLINK_TIMES;
use crate::process::ProcessManager;
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;
use alloc::string::ToString;
use alloc::vec::Vec;
use system_error::SystemError;

use crate::syscall::user_access::vfs_check_and_clone_cstr;

pub struct SysMknodatHandle;

impl Syscall for SysMknodatHandle {
    /// Returns the number of arguments this syscall takes (4).
    fn num_args(&self) -> usize {
        4
    }

    /// Handles the syscall
    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let dirfd = Self::dirfd(args);
        let path = Self::path(args);
        let mode_val = Self::mode(args);
        let dev = DeviceNumber::from(Self::dev(args));
        let path = vfs_check_and_clone_cstr(path, Some(MAX_PATHLEN))?
            .into_string()
            .map_err(|_| SystemError::EINVAL)?;

        // 解析 mode：提取文件类型和权限位
        // "Zero file type is equivalent to type S_IFREG." - mknod(2)
        let file_type_bits = mode_val & InodeMode::S_IFMT.bits();
        let perm_bits = mode_val & !InodeMode::S_IFMT.bits();

        let file_type = if file_type_bits == 0 {
            InodeMode::S_IFREG
        } else {
            InodeMode::from_bits(file_type_bits).ok_or(SystemError::EINVAL)?
        };

        // 应用 umask 到权限位
        // "In the absence of a default ACL, the permissions of the created node
        //  are (mode & ~umask)." - mknod(2)
        let pcb = ProcessManager::current_pcb();
        let umask = pcb.fs_struct().umask();
        let masked_perm = InodeMode::from_bits_truncate(perm_bits) & !umask;

        // 组合文件类型和 umask 后的权限
        let mode = file_type | masked_perm;

        let (mut current_inode, ret_path) = user_path_at(&pcb, dirfd, &path)?;
        let (name, parent) = rsplit_path(&ret_path);
        if let Some(parent) = parent {
            current_inode =
                current_inode.lookup_follow_symlink(parent, VFS_MAX_FOLLOW_SYMLINK_TIMES)?;
        }
        if name.is_empty() && dirfd != AtFlags::AT_FDCWD.bits() {
            return Err(SystemError::ENOENT);
        }

        if current_inode
            .lookup_follow_symlink(name, VFS_MAX_FOLLOW_SYMLINK_TIMES)
            .is_ok()
        {
            return Err(SystemError::EEXIST);
        }

        // 在解析出的父目录上进行 mknod
        current_inode.mknod(name, mode, dev)?;

        Ok(0)
    }

    /// Formats the syscall arguments for display/debugging purposes.
    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("dirfd", Self::dirfd(args).to_string()),
            FormattedSyscallParam::new("path", format!("{:#x}", Self::path(args) as usize)),
            FormattedSyscallParam::new("mode", Self::mode(args).to_string()),
            FormattedSyscallParam::new("dev", Self::dev(args).to_string()),
        ]
    }
}

impl SysMknodatHandle {
    /// Extracts the dir descriptor (dirfd) argument from syscall parameters.
    fn dirfd(args: &[usize]) -> i32 {
        args[0] as i32
    }
    /// Extracts the path argument from syscall parameters.
    fn path(args: &[usize]) -> *const u8 {
        args[1] as *const u8
    }
    /// Extracts the mode argument from syscall parameters.
    fn mode(args: &[usize]) -> u32 {
        args[2] as u32
    }
    /// Extracts the dev_t argument from syscall parameters.
    fn dev(args: &[usize]) -> u32 {
        args[3] as u32
    }
}

syscall_table_macros::declare_syscall!(SYS_MKNODAT, SysMknodatHandle);
