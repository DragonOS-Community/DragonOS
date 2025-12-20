use system_error::SystemError;

use crate::{
    filesystem::vfs::{
        fcntl::AtFlags,
        utils::{rsplit_path, user_path_at},
        FilePrivateData, FileType, NAME_MAX, VFS_MAX_FOLLOW_SYMLINK_TIMES,
    },
    libs::spinlock::SpinLock,
    process::ProcessManager,
};

use super::InodeMode;

pub fn do_symlinkat(from: &str, newdfd: Option<i32>, to: &str) -> Result<usize, SystemError> {
    let newdfd = match newdfd {
        Some(fd) => fd,
        None => AtFlags::AT_FDCWD.bits(),
    };

    // Linux 语义：
    // - symlink(2) 创建名为 `to` 的符号链接，其内容为 `from`
    // - `from`（目标字符串）不要求存在，也不应被解析/规范化
    // - 仅需解析并校验 `to` 的父目录存在且为目录，且 `to` 本身不存在
    //
    // TODO: 添加权限检查，确保进程拥有目标路径的权限（父目录 W+X）
    if to.is_empty() {
        return Err(SystemError::ENOENT);
    }

    let pcb = ProcessManager::current_pcb();

    // 得到新创建节点的父节点
    let (new_begin_inode, new_remain_path) = user_path_at(&pcb, newdfd, to)?;
    let (new_name, new_parent_path) = rsplit_path(&new_remain_path);

    // 检查文件名长度
    if new_name.len() > NAME_MAX {
        return Err(SystemError::ENAMETOOLONG);
    }

    // 当路径只有文件名（没有目录部分）时，new_parent_path 为 None，
    // 此时父目录就是 new_begin_inode 本身（即 cwd 或 dirfd 指向的目录）
    let new_parent = match new_parent_path {
        Some(parent_path) => {
            new_begin_inode.lookup_follow_symlink(parent_path, VFS_MAX_FOLLOW_SYMLINK_TIMES)?
        }
        None => new_begin_inode,
    };

    if new_parent.metadata()?.file_type != FileType::Dir {
        return Err(SystemError::ENOTDIR);
    }

    let new_inode =
        new_parent.create_with_data(new_name, FileType::SymLink, InodeMode::S_IRWXUGO, 0)?;

    let buf = from.as_bytes();
    let len = buf.len();
    new_inode.write_at(0, len, buf, SpinLock::new(FilePrivateData::Unused).lock())?;

    return Ok(0);
}
