use crate::filesystem::fsnotify::{self, FsEvent};
use crate::filesystem::vfs::mount::MountFSInode;
use crate::filesystem::vfs::permission::PermissionMask;
use crate::filesystem::vfs::syscall::RenameFlags;
use crate::filesystem::vfs::utils::is_ancestor;
use crate::filesystem::vfs::utils::rsplit_path;
use crate::filesystem::vfs::utils::user_path_at;
use crate::filesystem::vfs::SystemError;
use crate::filesystem::vfs::VFS_MAX_FOLLOW_SYMLINK_TIMES;
use crate::filesystem::vfs::{MAX_PATHLEN, NAME_MAX};
use crate::libs::casting::DowncastArc;
use crate::process::ProcessManager;
use crate::syscall::user_access::vfs_check_and_clone_cstr;
use alloc::sync::Arc;

struct RenameNotification<'a> {
    old_parent: &'a Arc<dyn crate::filesystem::vfs::IndexNode>,
    old_name: &'a str,
    new_parent: &'a Arc<dyn crate::filesystem::vfs::IndexNode>,
    new_name: &'a str,
    flags: RenameFlags,
    moved: &'a Arc<dyn crate::filesystem::vfs::IndexNode>,
    exchanged: Option<&'a Arc<dyn crate::filesystem::vfs::IndexNode>>,
    displaced: Option<&'a Arc<dyn crate::filesystem::vfs::IndexNode>>,
}

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

    // Linux rejects rename across mount objects even when two bind mounts
    // expose the same superblock and inode. Check this before final lookup and
    // same-inode no-op handling so a cross-mount alias cannot hide EXDEV.
    if let (Some(old_mount), Some(new_mount)) = (
        old_parent_inode.clone().downcast_arc::<MountFSInode>(),
        new_parent_inode.clone().downcast_arc::<MountFSInode>(),
    ) {
        if !old_mount.same_mount_ref(&new_mount) {
            return Err(SystemError::EXDEV);
        }
    }

    // 检查单个文件名长度
    if old_filename.len() > NAME_MAX || new_filename.len() > NAME_MAX {
        return Err(SystemError::ENAMETOOLONG);
    }

    if flags.contains(RenameFlags::NOREPLACE) && (new_filename == "." || new_filename == "..") {
        return Err(SystemError::EEXIST);
    }

    if old_filename == "." || old_filename == ".." || new_filename == "." || new_filename == ".." {
        return Err(SystemError::EBUSY);
    }

    let old_inode = old_parent_inode.lookup(old_filename)?;
    let old_inode_type = old_inode.metadata()?.file_type;

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
        let old_id = fsnotify::target_for_inode(&old_inode)?.id;
        let new_id = fsnotify::target_for_inode(new_inode)?.id;
        if old_id == new_id {
            return Ok(0);
        }
    }

    // 非 EXCHANGE：预先取出可能被覆盖的目标 inode（move_to 会静默销毁它），
    // 否则其上的 watch 会沦为持续产生事件的「幽灵 watch」。只有 ENOENT
    // 表示目标不存在；I/O、权限等 lookup 错误必须原样返回。
    let displaced = if !flags.contains(RenameFlags::EXCHANGE) {
        match new_parent_inode.find(new_filename) {
            Ok(inode) => Some(inode),
            Err(SystemError::ENOENT) => None,
            Err(error) => return Err(error),
        }
    } else {
        None
    };

    // Linux resolves the destination and handles NOREPLACE/same-inode no-op
    // before checking directory mutation permissions.
    if flags.contains(RenameFlags::NOREPLACE) && displaced.is_some() {
        return Err(SystemError::EEXIST);
    }

    if !flags.contains(RenameFlags::EXCHANGE)
        && Arc::ptr_eq(&old_parent_inode, &new_parent_inode)
        && old_filename == new_filename
    {
        return Ok(0);
    }

    if let Some(target) = displaced.as_ref() {
        let source_id = fsnotify::target_for_inode(&old_inode)?.id;
        let target_id = fsnotify::target_for_inode(target)?.id;
        if source_id == target_id {
            return Ok(0);
        }
    }

    // Ancestor traps are evaluated after a positive NOREPLACE destination and
    // same-inode no-op, matching Linux's lookup/error precedence.
    if old_inode_type == crate::filesystem::vfs::FileType::Dir
        && is_ancestor(&old_inode, &new_parent_inode)
    {
        return Err(SystemError::EINVAL);
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

    let notification = RenameNotification {
        old_parent: &old_parent_inode,
        old_name: old_filename,
        new_parent: &new_parent_inode,
        new_name: new_filename,
        flags,
        moved: &old_inode,
        exchanged: exchange_new_inode.as_ref(),
        displaced: displaced.as_ref(),
    };
    let notify = || notification.send();
    if let Some(mounted) = old_parent_inode.clone().downcast_arc::<MountFSInode>() {
        mounted.move_to_with_post_commit(
            old_filename,
            &new_parent_inode,
            new_filename,
            flags,
            notify,
        )?;
    } else {
        old_parent_inode.move_to(old_filename, &new_parent_inode, new_filename, flags)?;
        notify();
    }
    Ok(0)
}

impl RenameNotification<'_> {
    fn send(&self) {
        if self.flags.contains(RenameFlags::EXCHANGE) {
            // EXCHANGE：两个 inode 互换位置 → 两组配对事件、两个 cookie、双方各 IN_MOVE_SELF。
            // - old_inode: old_dir/old_name → new_dir/new_name（cookie1）
            // - new_inode: new_dir/new_name → old_dir/old_name（cookie2）
            let new_inode = self
                .exchanged
                .expect("RENAME_EXCHANGE requires target to exist (checked above)");
            let cookie1 = fsnotify::next_cookie();
            fsnotify::fsnotify(
                FsEvent::MOVED_FROM,
                Some((self.old_parent, self.old_name)),
                Some(self.moved),
                cookie1,
            );
            fsnotify::fsnotify(
                FsEvent::MOVED_TO,
                Some((self.new_parent, self.new_name)),
                Some(self.moved),
                cookie1,
            );
            fsnotify::fsnotify(FsEvent::MOVE_SELF, None, Some(self.moved), 0);
            let cookie2 = fsnotify::next_cookie();
            fsnotify::fsnotify(
                FsEvent::MOVED_FROM,
                Some((self.new_parent, self.new_name)),
                Some(new_inode),
                cookie2,
            );
            fsnotify::fsnotify(
                FsEvent::MOVED_TO,
                Some((self.old_parent, self.old_name)),
                Some(new_inode),
                cookie2,
            );
            fsnotify::fsnotify(FsEvent::MOVE_SELF, None, Some(new_inode), 0);
        } else {
            // 普通 rename（可能覆盖目标）：单 cookie 配对 MOVED_FROM/MOVED_TO + MOVE_SELF。
            let cookie = fsnotify::next_cookie();
            fsnotify::fsnotify(
                FsEvent::MOVED_FROM,
                Some((self.old_parent, self.old_name)),
                Some(self.moved),
                cookie,
            );
            fsnotify::fsnotify(
                FsEvent::MOVED_TO,
                Some((self.new_parent, self.new_name)),
                Some(self.moved),
                cookie,
            );
            fsnotify::fsnotify(FsEvent::MOVE_SELF, None, Some(self.moved), 0);
            // Replacing a target is part of the rename pair, not a parent DELETE.
            // Linux reports ATTRIB on the displaced inode; DELETE_SELF is tied to
            // the later dentry/inode detach lifecycle.
            if let Some(displaced) = self.displaced {
                fsnotify::fsnotify(FsEvent::ATTRIB, None, Some(displaced), 0);
            }
        }
    }
}
