//! System call handler for changing current working directory.

use system_error::SystemError;

use alloc::{string::String, vec::Vec};

use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_CHDIR;
use crate::filesystem::vfs::{FileType, MAX_PATHLEN, ROOT_INODE, VFS_MAX_FOLLOW_SYMLINK_TIMES};
use crate::process::ProcessManager;
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;
use crate::syscall::user_access::check_and_clone_cstr;

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
    ///   EACCESS    |        权限不足        
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

        let path = check_and_clone_cstr(path, Some(MAX_PATHLEN))?
            .into_string()
            .map_err(|_| SystemError::EINVAL)?;

        let proc = ProcessManager::current_pcb();
        // Copy path to kernel space to avoid some security issues
        let mut new_path = String::from("");
        if !path.is_empty() {
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
        }
        let inode =
            match ROOT_INODE().lookup_follow_symlink(&new_path, VFS_MAX_FOLLOW_SYMLINK_TIMES) {
                Err(_) => {
                    return Err(SystemError::ENOENT);
                }
                Ok(i) => i,
            };
        let metadata = inode.metadata()?;
        if metadata.file_type == FileType::Dir {
            proc.basic_mut().set_cwd(new_path);
            proc.fs_struct_mut().set_pwd(inode);
            return Ok(0);
        } else {
            return Err(SystemError::ENOTDIR);
        }
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
