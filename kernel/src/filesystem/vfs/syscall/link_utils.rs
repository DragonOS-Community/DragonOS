use crate::filesystem::vfs::syscall::AtFlags;
use crate::filesystem::vfs::utils::rsplit_path;
use crate::filesystem::vfs::utils::user_path_at;
use crate::filesystem::vfs::FileType;
use crate::filesystem::vfs::IndexNode;
use crate::filesystem::vfs::InodeMode;
use crate::filesystem::vfs::Metadata;
use crate::filesystem::vfs::SystemError;
use crate::filesystem::vfs::VFS_MAX_FOLLOW_SYMLINK_TIMES;
use crate::process::cred::CAPFlags;
use crate::process::cred::Cred;
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
    if new.ends_with('/') {
        return Err(SystemError::ENOENT);
    }

    // flag包含其他未规定值时返回EINVAL
    if !(AtFlags::AT_EMPTY_PATH | AtFlags::AT_SYMLINK_FOLLOW).contains(flags) {
        return Err(SystemError::EINVAL);
    }
    // TODO AT_EMPTY_PATH标志启用时，进行调用者CAP_DAC_READ_SEARCH或相似的检查
    let follow_last_symlink = flags.contains(AtFlags::AT_SYMLINK_FOLLOW);
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
            return Err(SystemError::ENOENT);
        }
    } else {
        let (old_begin_inode, old_remain_path) = user_path_at(&pcb, oldfd, old)?;
        old_begin_inode.lookup_follow_symlink2(
            &old_remain_path,
            VFS_MAX_FOLLOW_SYMLINK_TIMES,
            follow_last_symlink,
        )?
    };

    // old_inode为目录时返回EPERM
    if old_inode.metadata().unwrap().file_type == FileType::Dir {
        return Err(SystemError::EPERM);
    }
    // 硬链接安全检查
    may_linkat(&old_inode)?;

    // 得到新创建节点的父节点
    let (new_begin_inode, new_remain_path) = user_path_at(&pcb, newfd, new)?;
    let (new_name, new_parent_path) = rsplit_path(&new_remain_path);
    let new_parent = new_begin_inode
        .lookup_follow_symlink(new_parent_path.unwrap_or("/"), VFS_MAX_FOLLOW_SYMLINK_TIMES)?;

    // 被调用者利用downcast_ref判断两inode是否为同一文件系统
    return new_parent.link(new_name, &old_inode).map(|_| 0);
}

/// 检查是否允许创建硬链接（对应Linux的may_linkat）
///
/// 当protected_hardlinks启用时，阻止以下情况的硬链接创建：
/// - 源文件不是普通文件（特殊文件如FIFO、设备等）
/// - 源文件有setuid位
/// - 源文件有可执行的setgid位
/// - 调用者对源文件没有读写权限
///
/// 除非调用者是文件所有者或拥有CAP_FOWNER能力
fn may_linkat(old_inode: &Arc<dyn IndexNode>) -> Result<(), SystemError> {
    let metadata = old_inode.metadata()?;
    let cred = ProcessManager::current_pcb().cred();

    // TODO: 检查sysctl_protected_hardlinks是否启用
    // 目前假设始终启用（更安全的默认值）
    // 未来可添加sysctl支持：
    // if !sysctl_protected_hardlinks() {
    //     return Ok(());
    // }

    // 文件所有者可以创建硬链接
    if cred.is_owner(&metadata) {
        return Ok(());
    }

    // 拥有CAP_FOWNER能力可以创建硬链接
    if cred.has_capability(CAPFlags::CAP_FOWNER) {
        return Ok(());
    }

    // 检查是否是"安全的"硬链接源
    if !safe_hardlink_source(&metadata, &cred)? {
        return Err(SystemError::EPERM);
    }

    Ok(())
}

/// 判断硬链接源是否"安全"
fn safe_hardlink_source(
    metadata: &Metadata,
    cred: &Arc<Cred>,
) -> Result<bool, SystemError> {
    let mode = metadata.mode;
    let file_type = metadata.file_type;

    // 1. 必须是普通文件
    if file_type != FileType::File {
        return Ok(false);
    }

    // 2. 不能有setuid位
    if mode.contains(InodeMode::S_ISUID) {
        return Ok(false);
    }

    // 3. 不能是可执行的setgid文件
    if mode.contains(InodeMode::S_ISGID) && mode.contains(InodeMode::S_IXGRP) {
        return Ok(false);
    }

    // 4. 调用者必须有读写权限
    use crate::filesystem::vfs::PermissionMask;
    let need_perm = PermissionMask::MAY_READ.bits() | PermissionMask::MAY_WRITE.bits();
    if cred.inode_permission(metadata, need_perm).is_err() {
        return Ok(false);
    }

    Ok(true)
}
