use crate::filesystem::vfs::syscall::AtFlags;
use crate::filesystem::vfs::utils::rsplit_path;
use crate::filesystem::vfs::utils::user_path_at;
use crate::filesystem::vfs::FileType;
use crate::filesystem::vfs::IndexNode;
use crate::filesystem::vfs::SystemError;
use crate::filesystem::vfs::VFS_MAX_FOLLOW_SYMLINK_TIMES;
use crate::process::ProcessManager;
use alloc::sync::Arc;
/// **创建硬连接的系统调用**
///    
/// ## 参数
///
/// - 'oldfd': 用于解析源文件路径的文件描述符
/// - 'old': 源文件路径
/// - 'newfd': 用于解析新文件路径的文件描述符
/// - 'new': 新文件将创建的路径
/// - 'flags': 标志位，仅以位或方式包含AT_EMPTY_PATH和AT_SYMLINK_FOLLOW
///
///
pub fn do_linkat(
    oldfd: i32,
    old: &str,
    newfd: i32,
    new: &str,
    flags: AtFlags,
) -> Result<usize, SystemError> {
    // flag包含其他未规定值时返回EINVAL
    if !(AtFlags::AT_EMPTY_PATH | AtFlags::AT_SYMLINK_FOLLOW).contains(flags) {
        return Err(SystemError::EINVAL);
    }
    // TODO AT_EMPTY_PATH标志启用时，进行调用者CAP_DAC_READ_SEARCH或相似的检查
    let symlink_times = if flags.contains(AtFlags::AT_SYMLINK_FOLLOW) {
        0_usize
    } else {
        VFS_MAX_FOLLOW_SYMLINK_TIMES
    };
    let pcb = ProcessManager::current_pcb();

    // 得到源路径的inode
    let old_inode: Arc<dyn IndexNode> = if old.is_empty() {
        if flags.contains(AtFlags::AT_EMPTY_PATH) {
            // 在AT_EMPTY_PATH启用时，old可以为空，old_inode实际为oldfd所指文件，但该文件不能为目录。
            let binding = pcb.fd_table();
            let fd_table_guard = binding.read();
            let file = fd_table_guard
                .get_file_by_fd(oldfd)
                .ok_or(SystemError::EBADF)?;
            let old_inode = file.inode();
            old_inode
        } else {
            return Err(SystemError::ENONET);
        }
    } else {
        let (old_begin_inode, old_remain_path) = user_path_at(&pcb, oldfd, old)?;
        old_begin_inode.lookup_follow_symlink(&old_remain_path, symlink_times)?
    };

    // old_inode为目录时返回EPERM
    if old_inode.metadata().unwrap().file_type == FileType::Dir {
        return Err(SystemError::EPERM);
    }

    // 得到新创建节点的父节点
    let (new_begin_inode, new_remain_path) = user_path_at(&pcb, newfd, new)?;
    let (new_name, new_parent_path) = rsplit_path(&new_remain_path);
    let new_parent =
        new_begin_inode.lookup_follow_symlink(new_parent_path.unwrap_or("/"), symlink_times)?;

    // 被调用者利用downcast_ref判断两inode是否为同一文件系统
    return new_parent.link(new_name, &old_inode).map(|_| 0);
}
