use crate::filesystem::vfs::utils::is_ancestor_except_self;
use crate::filesystem::vfs::utils::rsplit_path;
use crate::filesystem::vfs::utils::user_path_at;
use crate::filesystem::vfs::SystemError;
use crate::filesystem::vfs::MAX_PATHLEN;
use crate::filesystem::vfs::VFS_MAX_FOLLOW_SYMLINK_TIMES;
use crate::process::ProcessManager;
use crate::syscall::user_access::check_and_clone_cstr;
/// # 修改文件名
///
///
/// ## 参数
///
/// - oldfd: 源文件夹文件描述符
/// - filename_from: 源文件路径
/// - newfd: 目标文件夹文件描述符
/// - filename_to: 目标文件路径
/// - flags: 标志位
///
///
/// ## 返回值
/// - Ok(返回值类型): 返回值的说明
/// - Err(错误值类型): 错误的说明
///
/// references: https://code.dragonos.org.cn/xref/linux-6.6.21/fs/namei.c#4913
pub fn do_renameat2(
    oldfd: i32,
    filename_from: *const u8,
    newfd: i32,
    filename_to: *const u8,
    flags: u32,
) -> Result<usize, SystemError> {
    let filename_from = check_and_clone_cstr(filename_from, Some(MAX_PATHLEN))
        .unwrap()
        .into_string()
        .map_err(|_| SystemError::EINVAL)?;
    let filename_to = check_and_clone_cstr(filename_to, Some(MAX_PATHLEN))
        .unwrap()
        .into_string()
        .map_err(|_| SystemError::EINVAL)?;
    // 文件名过长
    if filename_from.len() > MAX_PATHLEN || filename_to.len() > MAX_PATHLEN {
        return Err(SystemError::ENAMETOOLONG);
    }

    if filename_from == "/" || filename_to == "/" {
        return Err(SystemError::EBUSY);
    }

    let flags = Flags::from_bits_truncate(flags);
    if !flags.is_empty() {
        log::warn!("renameat2 flags {flags:?} not supported yet");
        return Err(SystemError::EINVAL);
    }
    //获取pcb，文件节点
    let pcb = ProcessManager::current_pcb();
    let (_old_inode_begin, old_remain_path) = user_path_at(&pcb, oldfd, &filename_from)?;
    let (_new_inode_begin, new_remain_path) = user_path_at(&pcb, newfd, &filename_to)?;
    //获取父目录
    let root_inode = ProcessManager::current_mntns().root_inode();
    let (old_filename, old_parent_path) = rsplit_path(&old_remain_path);
    let old_parent_inode = root_inode
        .lookup_follow_symlink(old_parent_path.unwrap_or("/"), VFS_MAX_FOLLOW_SYMLINK_TIMES)?;
    let (new_filename, new_parent_path) = rsplit_path(&new_remain_path);
    let new_parent_inode = root_inode
        .lookup_follow_symlink(new_parent_path.unwrap_or("/"), VFS_MAX_FOLLOW_SYMLINK_TIMES)?;

    if old_filename == "." || old_filename == ".." || new_filename == "." || new_filename == ".." {
        return Err(SystemError::EBUSY);
    }

    if flags.contains(Flags::NOREPLACE) {
        if let Ok(_) = new_parent_inode.lookup(new_filename) {
            return Err(SystemError::EEXIST);
        }
    }

    let old_inode = old_parent_inode.lookup(old_filename)?;
    if old_inode.metadata()?.file_type == crate::filesystem::vfs::FileType::Dir {
        // 仅当把目录移动到其自身或其子树下时拦截
        let old_meta = old_inode.metadata()?;
        let new_parent_meta = new_parent_inode.metadata()?;
        if old_meta.inode_id == new_parent_meta.inode_id
            || is_ancestor_except_self(&old_inode, &new_parent_inode)
        {
            return Err(SystemError::EINVAL);
        }
    }

    // 不要在这里检查 new_parent 是否是 old 的祖先：
    // 这会把同目录/向上移动的合法情况误判为 ENOTEMPTY。
    // 非空目录覆盖应由具体文件系统在 move_to/rename 实现中返回 ENOTEMPTY。

    old_parent_inode.move_to(old_filename, &new_parent_inode, new_filename)?;
    return Ok(0);
}

bitflags! {
    /// Flags used in the `renameat2` system call.
    ///
    /// Reference: <https://elixir.bootlin.com/linux/v6.16.3/source/include/uapi/linux/fcntl.h#L140-L143>.
    ///
    /// Reference: <https://man7.org/linux/man-pages/man2/renameat.2.html>.
    struct Flags: u32 {
        const NOREPLACE = 1 << 0;
        const EXCHANGE  = 1 << 1;
        const WHITEOUT  = 1 << 2;
    }
}
