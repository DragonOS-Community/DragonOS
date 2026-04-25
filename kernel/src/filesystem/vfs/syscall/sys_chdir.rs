//! System call handler for changing current working directory.

use system_error::SystemError;

use alloc::{string::String, vec::Vec};

use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_CHDIR;
use crate::filesystem::vfs::permission::PermissionMask;
use crate::filesystem::vfs::utils::user_path_at;
use crate::filesystem::vfs::{fcntl::AtFlags, FileType, MAX_PATHLEN, VFS_MAX_FOLLOW_SYMLINK_TIMES};
use crate::process::ProcessManager;
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;
use crate::syscall::user_access::vfs_check_and_clone_cstr;

/// System call handler for the `chdir` syscall
///
/// Changes the current working directory of the calling process.
pub struct SysChdirHandle;

impl Syscall for SysChdirHandle {
    /// Returns the number of arguments expected by the `chdir` syscall
    fn num_args(&self) -> usize {
        1
    }

    /// @brief 切换工作目录
    ///
    /// @param dest_path 目标路径
    ///
    /// @return   返回码  描述  
    ///      0       |          成功  
    ///         
    ///   EACCES    |        权限不足        
    ///
    ///    ELOOP     | 解析path时遇到路径循环
    ///
    /// ENAMETOOLONG |       路径名过长       
    ///
    ///    ENOENT    |  目标文件或目录不存在  
    ///
    ///    ENODIR    |  检索期间发现非目录项  
    ///
    ///    ENOMEM    |      系统内存不足      
    ///
    ///    EFAULT    |       错误的地址      
    ///  
    /// ENAMETOOLONG |        路径过长  
    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let path = Self::path(args);

        if path.is_null() {
            return Err(SystemError::EFAULT);
        }

        let path = vfs_check_and_clone_cstr(path, Some(MAX_PATHLEN))?
            .into_string()
            .map_err(|_| SystemError::EINVAL)?;
        let path = path.trim();
        if path.is_empty() {
            return Err(SystemError::ENOENT);
        }

        let proc = ProcessManager::current_pcb();

        // 解析目标 inode：绝对路径以进程 fs root 为起点；相对路径以 cwd inode 为起点
        let (inode_begin, remain_path) = user_path_at(&proc, AtFlags::AT_FDCWD.bits(), path)?;
        let inode =
            inode_begin.lookup_follow_symlink(&remain_path, VFS_MAX_FOLLOW_SYMLINK_TIMES)?;
        let metadata = inode.metadata()?;
        if metadata.file_type != FileType::Dir {
            return Err(SystemError::ENOTDIR);
        }

        crate::filesystem::vfs::permission::check_inode_permission(
            &inode,
            &metadata,
            PermissionMask::MAY_EXEC | PermissionMask::MAY_CHDIR,
        )?;

        // 维护一个“用户可见”的 cwd 字符串（用于 getcwd 的快速路径）
        // 注意：路径解析不再依赖该字符串，避免 chroot 后出现语义偏差。
        let mut new_path = String::from("");
        let cwd = match path.as_bytes()[0] {
            b'/' => String::from("/"),
            _ => proc.basic().cwd(),
        };
        let mut cwd_vec: Vec<_> = cwd.split('/').filter(|&x| !x.is_empty()).collect();
        let path_split = path.split('/').filter(|&x| !x.is_empty());
        for seg in path_split {
            if seg == ".." {
                cwd_vec.pop();
            } else if seg == "." {
                // 当前目录
            } else {
                cwd_vec.push(seg);
            }
        }
        for seg in cwd_vec {
            new_path.push('/');
            new_path.push_str(seg);
        }
        if new_path.is_empty() {
            new_path = String::from("/");
        }

        proc.basic_mut().set_cwd(new_path);
        proc.fs_struct_mut().set_pwd(inode);
        Ok(0)
    }

    /// Formats the syscall parameters for display/debug purposes
    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![FormattedSyscallParam::new(
            "path",
            format!("{:#x}", Self::path(args) as usize),
        )]
    }
}

impl SysChdirHandle {
    /// Extracts the path argument from syscall parameters
    fn path(args: &[usize]) -> *const u8 {
        args[0] as *const u8
    }
}

syscall_table_macros::declare_syscall!(SYS_CHDIR, SysChdirHandle);
