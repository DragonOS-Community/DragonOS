use crate::filesystem::fsnotify::{self, FsEvent};
use crate::filesystem::vfs::permission::PermissionMask;
use crate::filesystem::vfs::syscall::RenameFlags;
use crate::filesystem::vfs::utils::is_ancestor;
use crate::filesystem::vfs::utils::rsplit_path;
use crate::filesystem::vfs::utils::user_path_at;
use crate::filesystem::vfs::SystemError;
use crate::filesystem::vfs::VFS_MAX_FOLLOW_SYMLINK_TIMES;
use crate::filesystem::vfs::{MAX_PATHLEN, NAME_MAX};
use crate::process::ProcessManager;
use crate::syscall::user_access::vfs_check_and_clone_cstr;
use alloc::sync::Arc;
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
    let flags = RenameFlags::from_bits(flags).ok_or(SystemError::EINVAL)?;

    if flags.contains(RenameFlags::EXCHANGE)
        && (flags.contains(RenameFlags::NOREPLACE) || flags.contains(RenameFlags::WHITEOUT))
    {
        return Err(SystemError::EINVAL);
    }

    let filename_from = vfs_check_and_clone_cstr(filename_from, Some(MAX_PATHLEN))?
        .into_string()
        .map_err(|_| SystemError::EINVAL)?;
    let filename_to = vfs_check_and_clone_cstr(filename_to, Some(MAX_PATHLEN))?
        .into_string()
        .map_err(|_| SystemError::EINVAL)?;

    if filename_from == "/" || filename_to == "/" {
        return Err(SystemError::EBUSY);
    }

    //获取pcb，文件节点
    let pcb = ProcessManager::current_pcb();
    let (old_inode_begin, old_remain_path) = user_path_at(&pcb, oldfd, &filename_from)?;
    let (new_inode_begin, new_remain_path) = user_path_at(&pcb, newfd, &filename_to)?;
    let (old_filename, old_parent_path) = rsplit_path(&old_remain_path);
    let old_parent_inode = match old_parent_path {
        None => old_inode_begin,
        Some(p) => old_inode_begin.lookup_follow_symlink(p, VFS_MAX_FOLLOW_SYMLINK_TIMES)?,
    };
    let (new_filename, new_parent_path) = rsplit_path(&new_remain_path);
    let new_parent_inode = match new_parent_path {
        None => new_inode_begin,
        Some(p) => new_inode_begin.lookup_follow_symlink(p, VFS_MAX_FOLLOW_SYMLINK_TIMES)?,
    };

    // 检查单个文件名长度
    if old_filename.len() > NAME_MAX || new_filename.len() > NAME_MAX {
        return Err(SystemError::ENAMETOOLONG);
    }

    if flags.contains(RenameFlags::NOREPLACE) && (new_filename == "." || new_filename == "..") {
        return Err(SystemError::EEXIST);
    }

    // RENAME_EXCHANGE: 目标必须存在
    if flags.contains(RenameFlags::EXCHANGE) && new_parent_inode.find(new_filename).is_err() {
        return Err(SystemError::ENOENT);
    }

    if old_filename == "." || old_filename == ".." || new_filename == "." || new_filename == ".." {
        return Err(SystemError::EBUSY);
    }

    let old_inode = old_parent_inode.lookup(old_filename)?;
    let old_inode_type = old_inode.metadata()?.file_type;
    if old_inode_type == crate::filesystem::vfs::FileType::Dir {
        // 仅当把目录移动到其自身或其子树下时拦截
        if is_ancestor(&old_inode, &new_parent_inode) {
            return Err(SystemError::EINVAL);
        }
    }

    // RENAME_EXCHANGE 目标必须存在；预先 lookup 供事件投递复用（move_to 后原位置查不到）。
    let exchange_new_inode = if flags.contains(RenameFlags::EXCHANGE) {
        Some(new_parent_inode.lookup(new_filename)?)
    } else {
        None
    };
    if let Some(new_inode) = &exchange_new_inode {
        if new_inode.metadata()?.file_type == crate::filesystem::vfs::FileType::Dir
            && is_ancestor(new_inode, &old_parent_inode)
        {
            return Err(SystemError::EINVAL);
        }
    }

    // 不要在这里检查 new_parent 是否是 old 的祖先：
    // 这会把同目录/向上移动的合法情况误判为 ENOTEMPTY。
    // 非空目录覆盖应由具体文件系统在 move_to/rename 实现中返回 ENOTEMPTY。

    // 权限检查：根据 Linux 语义，rename 需要对源父目录和目标父目录都拥有写+搜索权限
    let old_parent_metadata = old_parent_inode.metadata()?;
    crate::filesystem::vfs::permission::check_inode_permission(
        &old_parent_inode,
        &old_parent_metadata,
        PermissionMask::MAY_WRITE | PermissionMask::MAY_EXEC,
    )?;

    let new_parent_metadata = new_parent_inode.metadata()?;
    crate::filesystem::vfs::permission::check_inode_permission(
        &new_parent_inode,
        &new_parent_metadata,
        PermissionMask::MAY_WRITE | PermissionMask::MAY_EXEC,
    )?;

    // 非 EXCHANGE：预先取出可能被覆盖的目标 inode（move_to 会静默销毁它），
    // 否则其上的 watch 会沦为持续产生事件的「幽灵 watch」。
    let displaced = if !flags.contains(RenameFlags::EXCHANGE) {
        new_parent_inode.find(new_filename).ok()
    } else {
        None
    };

    // 缓存 displaced 的 nlinks（move_to 前，displaced 还活着）。
    // move_to 会静默销毁 displaced 的目录项，之后的 metadata 可能失败或返回过时值。
    // nlinks <= 1 表示这是最后一个链接，覆盖后 inode 被销毁。
    let displaced_nlinks = displaced
        .as_ref()
        .and_then(|d| d.metadata().ok())
        .map(|m| m.nlinks);

    old_parent_inode.move_to(old_filename, &new_parent_inode, new_filename, flags)?;

    if flags.contains(RenameFlags::EXCHANGE) {
        // EXCHANGE：两个 inode 互换位置 → 两组配对事件、两个 cookie、双方各 IN_MOVE_SELF。
        // - old_inode: old_dir/old_name → new_dir/new_name（cookie1）
        // - new_inode: new_dir/new_name → old_dir/old_name（cookie2）
        let new_inode = exchange_new_inode
            .as_ref()
            .expect("RENAME_EXCHANGE requires target to exist (checked above)");
        let cookie1 = fsnotify::next_cookie();
        fsnotify::fsnotify(
            FsEvent::MOVED_FROM | FsEvent::MOVE_SELF,
            Some((&old_parent_inode, old_filename)),
            Some(&old_inode),
            cookie1,
        );
        fsnotify::fsnotify(
            FsEvent::MOVED_TO,
            Some((&new_parent_inode, new_filename)),
            Some(&old_inode),
            cookie1,
        );
        let cookie2 = fsnotify::next_cookie();
        fsnotify::fsnotify(
            FsEvent::MOVED_FROM | FsEvent::MOVE_SELF,
            Some((&new_parent_inode, new_filename)),
            Some(new_inode),
            cookie2,
        );
        fsnotify::fsnotify(
            FsEvent::MOVED_TO,
            Some((&old_parent_inode, old_filename)),
            Some(new_inode),
            cookie2,
        );
    } else {
        // 普通 rename（可能覆盖目标）：单 cookie 配对 MOVED_FROM/MOVED_TO + MOVE_SELF。
        // No-op rename：同父目录 + 同文件名 → 无实际变更，跳过事件投递。
        if Arc::ptr_eq(&old_parent_inode, &new_parent_inode) && old_filename == new_filename {
            return Ok(0);
        }
        let cookie = fsnotify::next_cookie();
        fsnotify::fsnotify(
            FsEvent::MOVED_FROM | FsEvent::MOVE_SELF,
            Some((&old_parent_inode, old_filename)),
            Some(&old_inode),
            cookie,
        );
        fsnotify::fsnotify(
            FsEvent::MOVED_TO,
            Some((&new_parent_inode, new_filename)),
            Some(&old_inode),
            cookie,
        );
        // 覆盖了已存在目标：补投 IN_DELETE（目标父目录）。
        // 仅当被覆盖的是不同 inode 且 nlinks 归零（最后一个硬链接）时才发
        // IN_DELETE_SELF（随之撤销 mark 并投递 IN_IGNORED）。
        // - 若 old_inode 与 displaced 是同一 inode（rename 覆盖自身别名），inode 未销毁；
        // - 若 displaced 有多个硬链接（nlinks > 1），覆盖一个链接后 inode 仍存活。
        if let Some(displaced) = &displaced {
            // 容错：metadata 读失败（如 FUSE）不影响 rename 的成功返回值；
            // 失败时按「不同 inode + nlinks 未知」处理（保守发 DELETE_SELF）。
            let same_inode = displaced
                .metadata()
                .ok()
                .zip(old_inode.metadata().ok())
                .map(|(d, o)| d.inode_id == o.inode_id)
                .unwrap_or(false);
            let mut mask = FsEvent::DELETE;
            if !same_inode && displaced_nlinks.is_none_or(|n| n <= 1) {
                mask |= FsEvent::DELETE_SELF;
            }
            fsnotify::fsnotify(
                mask,
                Some((&new_parent_inode, new_filename)),
                Some(displaced),
                0,
            );
        }
    }
    return Ok(0);
}
