use system_error::SystemError;

use crate::{
    filesystem::vfs::{
        FilePrivateData, FileType, VFS_MAX_FOLLOW_SYMLINK_TIMES,
        fcntl::AtFlags,
        utils::{rsplit_path, user_path_at},
    },
    libs::spinlock::SpinLock,
    process::ProcessManager,
};

use super::ModeType;

pub fn do_symlinkat(from: &str, newdfd: Option<i32>, to: &str) -> Result<usize, SystemError> {
    let newdfd = match newdfd {
        Some(fd) => fd,
        None => AtFlags::AT_FDCWD.bits(),
    };

    // TODO: 添加权限检查，确保进程拥有目标路径的权限
    let pcb = ProcessManager::current_pcb();
    let (old_begin_inode, old_remain_path) = user_path_at(&pcb, AtFlags::AT_FDCWD.bits(), from)?;
    // info!("old_begin_inode={:?}", old_begin_inode.metadata());
    let _ =
        old_begin_inode.lookup_follow_symlink(&old_remain_path, VFS_MAX_FOLLOW_SYMLINK_TIMES)?;

    // 得到新创建节点的父节点
    let (new_begin_inode, new_remain_path) = user_path_at(&pcb, newdfd, to)?;
    let (new_name, new_parent_path) = rsplit_path(&new_remain_path);
    let new_parent = new_begin_inode
        .lookup_follow_symlink(new_parent_path.unwrap_or("/"), VFS_MAX_FOLLOW_SYMLINK_TIMES)?;
    // info!("new_parent={:?}", new_parent.metadata());

    if new_parent.metadata()?.file_type != FileType::Dir {
        return Err(SystemError::ENOTDIR);
    }

    let new_inode = new_parent.create_with_data(
        new_name,
        FileType::SymLink,
        ModeType::from_bits_truncate(0o777),
        0,
    )?;

    let buf = old_remain_path.as_bytes();
    let len = buf.len();
    new_inode.write_at(0, len, buf, SpinLock::new(FilePrivateData::Unused).lock())?;
    return Ok(0);
}
