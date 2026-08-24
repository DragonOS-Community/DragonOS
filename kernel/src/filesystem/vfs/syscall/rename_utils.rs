use crate::filesystem::fsnotify::{self, FsEvent, FsNotifyTarget};
use crate::filesystem::vfs::mount::MountFSInode;
use crate::filesystem::vfs::permission::PermissionMask;
use crate::filesystem::vfs::syscall::RenameFlags;
use crate::filesystem::vfs::utils::is_ancestor;
use crate::filesystem::vfs::utils::rsplit_path;
use crate::filesystem::vfs::utils::user_path_at;
use crate::filesystem::vfs::SystemError;
use crate::filesystem::vfs::VFS_MAX_FOLLOW_SYMLINK_TIMES;
use crate::filesystem::vfs::{IndexNode, MAX_PATHLEN, NAME_MAX};
use crate::libs::casting::DowncastArc;
use crate::process::ProcessManager;
use crate::syscall::user_access::vfs_check_and_clone_cstr;
use alloc::sync::Arc;

struct RenameNotification<'a> {
    old_parent: FsNotifyTarget,
    old_name: &'a str,
    new_parent: FsNotifyTarget,
    new_name: &'a str,
    flags: RenameFlags,
    moved: FsNotifyTarget,
    exchanged: Option<FsNotifyTarget>,
    displaced: Option<FsNotifyTarget>,
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

    // RENAME_EXCHANGE 目标必须存在；预先 lookup 供事件投递复用（move_to 后原位置查不到）。
    let exchange_new_inode = if flags.contains(RenameFlags::EXCHANGE) {
        Some(new_parent_inode.lookup(new_filename)?)
    } else {
        None
    };

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

    if let Some(mounted) = old_parent_inode.clone().downcast_arc::<MountFSInode>() {
        let pre_commit = |moved: &Arc<dyn IndexNode>, target: Option<&Arc<dyn IndexNode>>| {
            validate_rename_commit(&old_parent_inode, &new_parent_inode, flags, moved, target)
        };
        let notify = |moved: &FsNotifyTarget, target: Option<&FsNotifyTarget>| {
            let Some(notification) = RenameNotification::from_targets(
                &old_parent_inode,
                old_filename,
                &new_parent_inode,
                new_filename,
                flags,
                moved.clone(),
                target.cloned(),
            ) else {
                return;
            };
            notification.send();
        };
        mounted.move_to_with_post_commit(
            old_filename,
            &new_parent_inode,
            new_filename,
            flags,
            pre_commit,
            notify,
        )?;
    } else {
        let target = if flags.contains(RenameFlags::EXCHANGE) {
            exchange_new_inode.as_ref()
        } else {
            displaced.as_ref()
        };
        validate_rename_commit(
            &old_parent_inode,
            &new_parent_inode,
            flags,
            &old_inode,
            target,
        )?;
        let outcome =
            old_parent_inode.move_to(old_filename, &new_parent_inode, new_filename, flags)?;
        if outcome != crate::filesystem::vfs::RenameOutcome::NoOp {
            let target = if flags.contains(RenameFlags::EXCHANGE) {
                exchange_new_inode.as_ref()
            } else {
                displaced.as_ref()
            };
            if let Some(notification) = RenameNotification::from_inodes(
                &old_parent_inode,
                old_filename,
                &new_parent_inode,
                new_filename,
                flags,
                &old_inode,
                target,
            ) {
                notification.send();
            }
        }
    }
    Ok(0)
}

fn validate_rename_commit(
    old_parent: &Arc<dyn IndexNode>,
    new_parent: &Arc<dyn IndexNode>,
    flags: RenameFlags,
    moved: &Arc<dyn IndexNode>,
    target: Option<&Arc<dyn IndexNode>>,
) -> Result<(), SystemError> {
    if flags.contains(RenameFlags::EXCHANGE) {
        if let Some(target) = target {
            if target.metadata()?.file_type == crate::filesystem::vfs::FileType::Dir
                && is_ancestor(target, old_parent)
            {
                return Err(SystemError::EINVAL);
            }
        }
    }
    if moved.metadata()?.file_type == crate::filesystem::vfs::FileType::Dir
        && is_ancestor(moved, new_parent)
    {
        return Err(SystemError::EINVAL);
    }

    let old_parent_metadata = old_parent.metadata()?;
    crate::filesystem::vfs::permission::check_inode_permission(
        old_parent,
        &old_parent_metadata,
        PermissionMask::MAY_WRITE | PermissionMask::MAY_EXEC,
    )?;
    let new_parent_metadata = new_parent.metadata()?;
    crate::filesystem::vfs::permission::check_inode_permission(
        new_parent,
        &new_parent_metadata,
        PermissionMask::MAY_WRITE | PermissionMask::MAY_EXEC,
    )
}

impl RenameNotification<'_> {
    fn from_targets<'a>(
        old_parent: &Arc<dyn IndexNode>,
        old_name: &'a str,
        new_parent: &Arc<dyn IndexNode>,
        new_name: &'a str,
        flags: RenameFlags,
        moved: FsNotifyTarget,
        target: Option<FsNotifyTarget>,
    ) -> Option<RenameNotification<'a>> {
        let (exchanged, displaced) = if flags.contains(RenameFlags::EXCHANGE) {
            (target, None)
        } else {
            (None, target)
        };
        Some(RenameNotification {
            old_parent: fsnotify::target_for_inode(old_parent).ok()?,
            old_name,
            new_parent: fsnotify::target_for_inode(new_parent).ok()?,
            new_name,
            flags,
            moved,
            exchanged,
            displaced,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn from_inodes<'a>(
        old_parent: &Arc<dyn IndexNode>,
        old_name: &'a str,
        new_parent: &Arc<dyn IndexNode>,
        new_name: &'a str,
        flags: RenameFlags,
        moved: &Arc<dyn IndexNode>,
        target: Option<&Arc<dyn IndexNode>>,
    ) -> Option<RenameNotification<'a>> {
        Self::from_targets(
            old_parent,
            old_name,
            new_parent,
            new_name,
            flags,
            fsnotify::target_for_inode(moved).ok()?,
            target.and_then(|target| fsnotify::target_for_inode(target).ok()),
        )
    }

    fn send(&self) {
        if self.flags.contains(RenameFlags::EXCHANGE) {
            // EXCHANGE：两个 inode 互换位置 → 两组配对事件、两个 cookie、双方各 IN_MOVE_SELF。
            // - old_inode: old_dir/old_name → new_dir/new_name（cookie1）
            // - new_inode: new_dir/new_name → old_dir/old_name（cookie2）
            let new_inode = self
                .exchanged
                .as_ref()
                .expect("RENAME_EXCHANGE requires target to exist (checked above)");
            let cookie1 = fsnotify::next_cookie();
            fsnotify::fsnotify_targets(
                FsEvent::MOVED_FROM,
                Some((&self.old_parent, self.old_name)),
                Some(&self.moved),
                cookie1,
                false,
            );
            fsnotify::fsnotify_targets(
                FsEvent::MOVED_TO,
                Some((&self.new_parent, self.new_name)),
                Some(&self.moved),
                cookie1,
                false,
            );
            fsnotify::fsnotify_targets(FsEvent::MOVE_SELF, None, Some(&self.moved), 0, false);
            let cookie2 = fsnotify::next_cookie();
            fsnotify::fsnotify_targets(
                FsEvent::MOVED_FROM,
                Some((&self.new_parent, self.new_name)),
                Some(new_inode),
                cookie2,
                false,
            );
            fsnotify::fsnotify_targets(
                FsEvent::MOVED_TO,
                Some((&self.old_parent, self.old_name)),
                Some(new_inode),
                cookie2,
                false,
            );
            fsnotify::fsnotify_targets(FsEvent::MOVE_SELF, None, Some(new_inode), 0, false);
        } else {
            // 普通 rename（可能覆盖目标）：单 cookie 配对 MOVED_FROM/MOVED_TO + MOVE_SELF。
            let cookie = fsnotify::next_cookie();
            fsnotify::fsnotify_targets(
                FsEvent::MOVED_FROM,
                Some((&self.old_parent, self.old_name)),
                Some(&self.moved),
                cookie,
                false,
            );
            fsnotify::fsnotify_targets(
                FsEvent::MOVED_TO,
                Some((&self.new_parent, self.new_name)),
                Some(&self.moved),
                cookie,
                false,
            );
            // Replacing a target is part of the rename pair, not a parent DELETE.
            // Linux reports ATTRIB on the displaced inode; DELETE_SELF is tied to
            // the later dentry/inode detach lifecycle.
            if let Some(displaced) = self.displaced.as_ref() {
                fsnotify::fsnotify_targets(FsEvent::ATTRIB, None, Some(displaced), 0, false);
            }
            fsnotify::fsnotify_targets(FsEvent::MOVE_SELF, None, Some(&self.moved), 0, false);
        }
    }
}
