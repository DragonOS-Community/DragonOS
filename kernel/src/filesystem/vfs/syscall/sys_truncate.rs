use crate::arch::syscall::nr::SYS_TRUNCATE;
use crate::{
    arch::interrupt::TrapFrame,
    filesystem::vfs::{
        fcntl::AtFlags, utils::user_path_at, IndexNode, MAX_PATHLEN, VFS_MAX_FOLLOW_SYMLINK_TIMES,
    },
    process::ProcessManager,
    syscall::{
        table::{FormattedSyscallParam, Syscall},
        user_access::check_and_clone_cstr,
    },
};

use alloc::sync::Arc;
use alloc::vec::Vec;
use system_error::SystemError;

use crate::filesystem::vfs::vcore::vfs_truncate;

/// # truncate(path, length)
///
/// 基于路径调整文件大小：
/// - 跟随符号链接定位最终 inode。
/// - 目录返回 EISDIR；非普通文件返回 EINVAL。
/// - 只读挂载点返回 EROFS。
/// - 调用 inode.resize(length)。
pub struct SysTruncateHandle;

impl Syscall for SysTruncateHandle {
    fn num_args(&self) -> usize {
        2
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let path_ptr = args[0] as *const u8;
        let length = args[1];

        if length > 1024 * 1024 {
            return Err(SystemError::EFBIG);
        }
        // 复制并校验用户态路径
        let path = check_and_clone_cstr(path_ptr, Some(MAX_PATHLEN))?;
        let path = path.to_str().map_err(|_| SystemError::EINVAL)?;

        // 解析起始 inode 与剩余路径
        let (begin_inode, remain_path) = user_path_at(
            &ProcessManager::current_pcb(),
            AtFlags::AT_FDCWD.bits(),
            path,
        )?;

        // 跟随符号链接解析最终目标 inode
        let target: Arc<dyn IndexNode> = begin_inode
            .lookup_follow_symlink(remain_path.as_str(), VFS_MAX_FOLLOW_SYMLINK_TIMES)?;

        vfs_truncate(target, length)?;
        Ok(0)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("path", format!("{:#x}", args[0])),
            FormattedSyscallParam::new("length", format!("{:#x}", args[1])),
        ]
    }
}

syscall_table_macros::declare_syscall!(SYS_TRUNCATE, SysTruncateHandle);
