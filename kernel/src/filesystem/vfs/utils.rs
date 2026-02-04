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

        if cur_id == ancestor_id {
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

        if parent_id == cur_id {
            break;
        }
        next_node = Some(parent);
    }

    false
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
