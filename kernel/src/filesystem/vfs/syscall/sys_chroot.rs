//! System call handler for chroot(2).
//!
//! Linux 语义要点：
//! - 需要 CAP_SYS_CHROOT，否则 EPERM
//! - path 必须存在且为目录，否则 ENOENT/ENOTDIR
//! - 需要对目标目录具备搜索(执行)权限，否则 EACCES
//! - 成功后仅改变调用进程的 fs root，不改变 cwd（因此 cwd 可能落在新 root 外）

use alloc::vec::Vec;
use system_error::SystemError;

use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_CHROOT;
use crate::filesystem::vfs::permission::PermissionMask;
use crate::filesystem::vfs::{
    utils::user_path_at, FileType, MAX_PATHLEN, VFS_MAX_FOLLOW_SYMLINK_TIMES,
};
use crate::process::cred::CAPFlags;
use crate::process::ProcessManager;
use crate::syscall::table::{FormattedSyscallParam, Syscall};
use crate::syscall::user_access::vfs_check_and_clone_cstr;

pub struct SysChrootHandle;

impl Syscall for SysChrootHandle {
    fn num_args(&self) -> usize {
        1
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let path_ptr = Self::path(args);
        if path_ptr.is_null() {
            return Err(SystemError::EFAULT);
        }

        let path = vfs_check_and_clone_cstr(path_ptr, Some(MAX_PATHLEN))?
            .into_string()
            .map_err(|_| SystemError::EINVAL)?;
        let path = path.trim();
        if path.is_empty() {
            return Err(SystemError::ENOENT);
        }

        let pcb = ProcessManager::current_pcb();
        let cred = pcb.cred();

        // 权限：CAP_SYS_CHROOT
        if !cred.has_capability(CAPFlags::CAP_SYS_CHROOT) {
            return Err(SystemError::EPERM);
        }

        // 解析路径（相对路径基于 cwd inode，绝对路径基于进程 fs root）
        let (inode_begin, resolved_path) = user_path_at(
            &pcb,
            crate::filesystem::vfs::fcntl::AtFlags::AT_FDCWD.bits(),
            path,
        )?;
        let target =
            inode_begin.lookup_follow_symlink(&resolved_path, VFS_MAX_FOLLOW_SYMLINK_TIMES)?;

        let meta = target.metadata()?;
        if meta.file_type != FileType::Dir {
            return Err(SystemError::ENOTDIR);
        }

        crate::filesystem::vfs::permission::check_inode_permission(
            &target,
            &meta,
            PermissionMask::MAY_EXEC,
        )?;

        // 更新进程 fs root；不改变 cwd（Linux 行为）
        pcb.fs_struct_mut().set_root(target);
        Ok(0)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![FormattedSyscallParam::new(
            "path",
            format!("{:#x}", Self::path(args) as usize),
        )]
    }
}

impl SysChrootHandle {
    fn path(args: &[usize]) -> *const u8 {
        args[0] as *const u8
    }
}

syscall_table_macros::declare_syscall!(SYS_CHROOT, SysChrootHandle);
