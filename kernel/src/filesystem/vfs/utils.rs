use core::cmp::Ordering;
use core::fmt::{self, Debug};
use core::hash::Hash;

use alloc::{string::String, sync::Arc};
use system_error::SystemError;

use crate::process::cred::{Cred, Kgid};
use crate::process::ProcessControlBlock;

use super::{fcntl::AtFlags, FileType, IndexNode, InodeMode};

/// @brief 切分路径字符串，返回最左侧那一级的目录名和剩余的部分。
///
/// 举例：对于 /123/456/789/   本函数返回的第一个值为123, 第二个值为456/789
#[allow(dead_code)]
pub fn split_path(path: &str) -> (&str, Option<&str>) {
    let mut path_split: core::str::SplitN<&str> = path.trim_matches('/').splitn(2, "/");
    let comp = path_split.next().unwrap_or("");
    let rest_opt = path_split.next();

    return (comp, rest_opt);
}

/// @brief 切分路径字符串，返回最右侧那一级的目录名和剩余的部分。
///
/// 举例：对于 /123/456/789/   本函数返回的第一个值为789, 第二个值为123/456
pub fn rsplit_path(path: &str) -> (&str, Option<&str>) {
    let mut path_split: core::str::RSplitN<&str> = path.trim_matches('/').rsplitn(2, "/");
    let comp = path_split.next().unwrap_or("");
    let rest_opt = path_split.next();

    return (comp, rest_opt);
}

/// 根据dirfd和path，计算接下来开始lookup的inode和剩余的path
///
/// ## 返回值
///
/// 返回值为(需要执行lookup的inode, 剩余的path)
pub fn user_path_at(
    pcb: &Arc<ProcessControlBlock>,
    dirfd: i32,
    path: &str,
) -> Result<(Arc<dyn IndexNode>, String), SystemError> {
    // Linux 语义：
    // - 绝对路径从进程的 fs root 开始解析（chroot 会改变它）
    // - 相对路径默认从进程的 cwd(pwd inode) 开始解析
    // - dirfd != AT_FDCWD 时，从对应目录 fd 开始解析

    let ret_path = String::from(path);

    // 空路径：交由上层 syscall 自己决定（open/chroot 等对空串语义不同）
    if path.is_empty() {
        return Ok((pcb.pwd_inode(), ret_path));
    }

    // 绝对路径：从进程 root 开始
    if path.as_bytes()[0] == b'/' {
        let root = pcb.fs_struct().root();
        // log::debug!("[user_path_at] absolute path '{}', root fs={}", path, root.fs().name());
        return Ok((root, ret_path));
    }

    // 相对路径：dirfd 优先，否则用 cwd
    if dirfd != AtFlags::AT_FDCWD.bits() {
        let binding = pcb.fd_table();
        let fd_table_guard = binding.read();
        let file = fd_table_guard
            .get_file_by_fd(dirfd)
            .ok_or(SystemError::EBADF)?;

        // drop guard 以避免无法调度的问题
        drop(fd_table_guard);

        // 如果dirfd不是目录，则返回错误码ENOTDIR
        if file.file_type() != FileType::Dir {
            return Err(SystemError::ENOTDIR);
        }
        return Ok((file.inode(), ret_path));
    }

    Ok((pcb.pwd_inode(), ret_path))
}

/// 检查 ancestor 是否是 node 的祖先节点
///
/// 通过向上遍历 node 的父目录链，检查是否能到达 ancestor。
/// 这用于验证一个目录是否在另一个目录之下（如 pivot_root 中检查 put_old 是否在 new_root 之下）。
///
/// # 参数
///
/// - `ancestor`: 可能的祖先节点
/// - `node`: 要检查的节点
///
/// # 返回值
///
/// 如果 ancestor 是 node 的祖先（包括 ancestor == node 的情况），返回 true；否则返回 false。
///
/// # 注意
///
/// - 此函数通过 inode ID 和文件系统指针进行比较
/// - 检查会在到达根目录或发生错误时停止
/// - 不检查跨文件系统的边界（即需要同时在同一文件系统中）
pub fn is_ancestor(ancestor: &Arc<dyn IndexNode>, node: &Arc<dyn IndexNode>) -> bool {
    let ancestor_id = match ancestor.metadata() {
        Ok(m) => m.inode_id,
        Err(_) => return false,
    };

    let mut next_node: Option<Arc<dyn IndexNode>> = Some(node.clone());
    while let Some(current) = next_node {
        let cur_id = match current.metadata() {
            Ok(m) => m.inode_id,
            Err(_) => break,
        };

        // 同时检查 inode ID 和文件系统指针，确保是同一个文件系统中的同一个 inode
        if cur_id == ancestor_id && Arc::ptr_eq(&current.fs(), &ancestor.fs()) {
            return true;
        }

        let parent = match current.parent() {
            Ok(p) => p,
            Err(_) => break, // 没有父节点，到达根或错误，停止循环
        };

        let parent_id = match parent.metadata() {
            Ok(m) => m.inode_id,
            Err(_) => break,
        };

        // 如果父节点的 inode ID 和当前节点相同，说明已经到达根目录
        if parent_id == cur_id {
            break;
        }
        next_node = Some(parent);
    }

    false
}

/// is_ancestor() 遍历父目录的最大深度
///
/// 用于防止在文件系统损坏或存在循环引用时出现无限循环。
/// 1000 层对于正常使用场景来说绰绰有余（Linux 默认的链接数限制也远低于此）。
pub const MAX_ANCESTOR_TRAVERSAL_DEPTH: u32 = 1000;

/// 检查 ancestor 是否是 node 的祖先节点（带深度限制的版本）
///
/// 与 is_ancestor() 类似，但增加最大遍历深度限制以防止潜在的无限循环。
/// 返回 Result 类型，可以传递错误信息。
///
/// # 参数
///
/// - `ancestor`: 可能的祖先节点
/// - `node`: 要检查的节点
///
/// # 返回值
///
/// - `Ok(true)`: ancestor 是 node 的祖先
/// - `Ok(false)`: ancestor 不是 node 的祖先
/// - `Err(SystemError)`: 遍历过程中发生错误
///
/// # 注意
///
/// 此函数会限制遍历深度为 MAX_ANCESTOR_TRAVERSAL_DEPTH 层。
/// 如果超过此深度仍未找到祖先，将返回 Ok(false)。
pub fn is_ancestor_limited(
    ancestor: &Arc<dyn IndexNode>,
    node: &Arc<dyn IndexNode>,
) -> Result<bool, SystemError> {
    let ancestor_meta = ancestor.metadata()?;
    let ancestor_id = ancestor_meta.inode_id;
    let ancestor_fs = ancestor.fs();

    let mut current = node.clone();

    // 最多向上遍历 MAX_ANCESTOR_TRAVERSAL_DEPTH 层，防止循环引用
    for _i in 0..MAX_ANCESTOR_TRAVERSAL_DEPTH {
        let current_meta = current.metadata()?;

        // 检查是否到达 ancestor（同时检查 inode ID 和文件系统）
        if current_meta.inode_id == ancestor_id && Arc::ptr_eq(&current.fs(), &ancestor_fs) {
            return Ok(true);
        }

        // 尝试向上移动到父目录
        match current.parent() {
            Ok(parent) => {
                // 如果 parent 就是 current 本身，说明已经到达根目录
                if Arc::ptr_eq(&parent, &current) {
                    break;
                }
                current = parent;
            }
            Err(_) => {
                // 没有父目录了，到达根目录
                break;
            }
        }
    }

    // 最后再检查一次根目录是否是 ancestor
    let root_meta = current.metadata()?;
    Ok(root_meta.inode_id == ancestor_id && Arc::ptr_eq(&current.fs(), &ancestor_fs))
}

/// Directory Name
/// 可以用来作为原地提取目录名及比较的
/// Dentry的对标（x
///
/// 简单的生成一个新的DName，以实现简单的RCU
#[derive(Debug)]
pub struct DName(pub Arc<String>);

impl PartialEq for DName {
    fn eq(&self, other: &Self) -> bool {
        return *self.0 == *other.0;
    }
}
impl Eq for DName {}

impl Hash for DName {
    fn hash<H: core::hash::Hasher>(&self, state: &mut H) {
        self.0.hash(state)
    }
}

impl PartialOrd for DName {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for DName {
    fn cmp(&self, other: &Self) -> Ordering {
        return self.0.cmp(&other.0);
    }
}

impl Default for DName {
    fn default() -> Self {
        Self(Arc::new(String::new()))
    }
}

impl From<String> for DName {
    fn from(value: String) -> Self {
        Self(Arc::from(value))
    }
}

impl From<&str> for DName {
    fn from(value: &str) -> Self {
        Self(Arc::from(String::from(value)))
    }
}

impl Clone for DName {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl fmt::Display for DName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl AsRef<str> for DName {
    fn as_ref(&self) -> &str {
        self.0.as_str()
    }
}

/// 检查调用者是否在指定的组内
///
/// # 参数
///
/// - `cred`: 调用者的凭证
/// - `gid`: 要检查的组 ID
///
/// # 返回值
///
/// 如果调用者在指定组内，返回 `true`；否则返回 `false`
pub fn is_caller_in_group(cred: &Arc<Cred>, gid: usize) -> bool {
    let kgid = Kgid::from(gid);
    let mut in_group = cred.fsgid.data() == gid
        || cred.gid.data() == gid
        || cred.egid.data() == gid
        || cred.getgroups().contains(&kgid);

    if let Some(info) = cred.group_info.as_ref() {
        in_group |= info.gids.contains(&kgid);
    }

    in_group
}

/// 判断是否应该清除 sgid 位（遵循 Linux setattr_should_drop_sgid 语义）
///
/// # 参数
///
/// - `mode`: 文件的权限模式
/// - `gid`: 要检查的组 ID
/// - `cred`: 调用者的凭证
///
/// # 返回值
///
/// 如果应该清除 sgid 位，返回 `true`；否则返回 `false`
///
/// # Linux 语义
///
/// - 若 S_IXGRP 置位：无条件清除（这是"真正的"SGID 可执行文件）
/// - 否则（mandatory locking 语义）：仅当调用者不在文件所属组时清除
pub fn should_remove_sgid(mode: InodeMode, gid: usize, cred: &Arc<Cred>) -> bool {
    if !mode.contains(InodeMode::S_ISGID) {
        return false;
    }

    if mode.contains(InodeMode::S_IXGRP) {
        // S_IXGRP 置位：无条件清除 sgid
        return true;
    }

    // mandatory locking 语义：仅当调用者不在文件所属组时清除
    !is_caller_in_group(cred, gid)
}

/// 判断 chown 操作中是否应该清除 sgid 位
///
/// # 参数
///
/// - `mode`: 文件的权限模式
/// - `old_gid`: chown 前的原组 ID
/// - `current_gid`: 当前调用者的 gid
/// - `cred`: 调用者的凭证
/// - `group_info`: 调用者的组信息
///
/// # 返回值
///
/// 如果应该清除 sgid 位，返回 `true`；否则返回 `false`
///
/// # Linux 语义
///
/// - 若 S_IXGRP 置位：无条件清除 sgid
/// - 否则（mandatory locking 语义）：仅当调用者不在原文件所属组且无 CAP_FSETID 时清除
pub fn should_remove_sgid_on_chown(
    mode: InodeMode,
    old_gid: usize,
    current_gid: usize,
    cred: &Arc<Cred>,
    group_info: &crate::process::cred::GroupInfo,
) -> bool {
    use crate::process::cred::CAPFlags;

    if !mode.contains(InodeMode::S_ISGID) {
        return false;
    }

    if mode.contains(InodeMode::S_IXGRP) {
        // S_IXGRP 置位：无条件清除 sgid
        return true;
    }

    // 注意：Linux 这里检查的是"原 inode gid"，在 notify_change 前尚未更新。
    let kgid = Kgid::from(old_gid);
    let in_group = cred.fsgid.data() == old_gid
        || cred.egid.data() == old_gid
        || current_gid == old_gid
        || cred.getgroups().contains(&kgid)
        || group_info.gids.contains(&kgid)
        || cred.has_capability(CAPFlags::CAP_FSETID);

    // 仅当调用者不在原文件所属组且无 CAP_FSETID 时清除
    !in_group
}
