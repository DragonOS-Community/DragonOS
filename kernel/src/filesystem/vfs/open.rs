use crate::{
    process::ProcessManager,
    syscall::{user_access::check_and_clone_cstr, SystemError},
};

use super::{
    fcntl::AtFlags, syscall::ModeType, utils::user_path_at, MAX_PATHLEN,
    VFS_MAX_FOLLOW_SYMLINK_TIMES,
};

pub(super) fn do_faccessat(
    dirfd: i32,
    path: *const u8,
    mode: ModeType,
    flags: u32,
) -> Result<usize, SystemError> {
    if (mode.bits() & (!ModeType::S_IRWXO.bits())) != 0 {
        return Err(SystemError::EINVAL);
    }

    if (flags
        & (!((AtFlags::AT_EACCESS | AtFlags::AT_SYMLINK_NOFOLLOW | AtFlags::AT_EMPTY_PATH).bits()
            as u32)))
        != 0
    {
        return Err(SystemError::EINVAL);
    }

    // let follow_symlink = flags & AtFlags::AT_SYMLINK_NOFOLLOW.bits() as u32 == 0;

    let path = check_and_clone_cstr(path, Some(MAX_PATHLEN))?;

    if path.len() == 0 {
        return Err(SystemError::EINVAL);
    }

    let (inode, path) = user_path_at(&ProcessManager::current_pcb(), dirfd, path)?;

    // 如果找不到文件，则返回错误码ENOENT
    let _inode = inode.lookup_follow_symlink(path.as_str(), VFS_MAX_FOLLOW_SYMLINK_TIMES)?;

    // todo: 接着完善（可以借鉴linux 6.1.9的do_faccessat）
    return Ok(0);
}

pub fn do_fchmodat(dirfd: i32, path: *const u8, _mode: ModeType) -> Result<usize, SystemError> {
    let path = check_and_clone_cstr(path, Some(MAX_PATHLEN))?;

    if path.len() == 0 {
        return Err(SystemError::EINVAL);
    }

    let (inode, path) = user_path_at(&ProcessManager::current_pcb(), dirfd, path)?;

    // 如果找不到文件，则返回错误码ENOENT
    let _inode = inode.lookup_follow_symlink(path.as_str(), VFS_MAX_FOLLOW_SYMLINK_TIMES)?;

    kwarn!("do_fchmodat: not implemented yet\n");
    // todo: 真正去改变文件的权限

    return Ok(0);
}
