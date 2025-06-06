use system_error::SystemError;

use crate::syscall::table::Syscall;

use super::{FileType, MAX_PATHLEN};
use crate::process::ProcessManager;
/// SYS_TRUNCATE系统调用Handler
pub struct SysTruncateHandle;

impl Syscall for SysTruncateHandle {
    /// 返回参数个数
    fn num_args(&self) -> usize {
        2
    }
    fn handle(&self, args: &[usize], from_user: bool) -> Result<usize, SystemError> {
        let path_ptr = Self::path_ptr(args);
        let len = Self::len(args);
        let res = do_truncate(path_ptr, len);
        res
    }
    fn entry_format(
        &self,
        args: &[usize],
    ) -> std::vec::Vec<crate::syscall::table::FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("path", format!("{:#x}", Self::path(args) as usize)),
            FormattedSyscallParam::new("len", Self::len(args).to_string()),
        ]
    }
}

impl SysTruncateHandle {
    /// Extracts the path_ptr from syscall arguments
    fn path_ptr(args: &[usize]) -> *const u8 {
        args[0] as *const u8
    }
    /// Extracts the length from syscall arguments
    fn len(args: &[usize]) -> usize {
        args[1]
    }
}

syscall_table_macros::declare_syscall!(SYS_TRUNCATE, SysTruncateHandle);

/// # SYS_TRUNCATE 系统调用的实际执行函数
/// 改变文件大小.
/// 如果文件大小大于原来的大小，那么文件的内容将会被扩展到指定的大小，新的空间将会用0填充.
/// 如果文件大小小于原来的大小，那么文件的内容将会被截断到指定的大小.
/// 与SYS_FTRUNCATE的区别在于SYS_TRUNCATE不需要文件描述符 仅使用路径标识文件
/// ## 参数
/// - `path`: 文件路径
/// - `len`: 文件大小
pub fn do_truncate(path: *const u8, length: usize) -> Result<usize, SystemError> {
    let path: String = check_and_clone_cstr(path, Some(MAX_PATHLEN))?
        .into_string()
        .map_err(|_| SystemError::EINVAL)?;
    let pwd = ProcessManager::current_pcb().pwd_inode();

    let inode = pwd.lookup(&path)?;
    // 获取inode元数据
    let metadata = inode.metadata()?;
    // 若是目录返回EISDIR 其他的返回EINVAL
    if metadata.file_type == FileType::Dir {
        return Err(SystemError::EISDIR);
    }
    if metadata.file_type != FileType::File {
        return Err(SystemError::EINVAL);
    }
    // TODO!: 添加权限检查
    inode.truncate(length)?;
    Ok(0)
}
