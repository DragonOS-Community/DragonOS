use system_error::SystemError;

use crate::{
    filesystem::vfs::file::{FileDescriptorVec, FileMode},
    libs::rwlock::RwLockWriteGuard,
};

pub fn do_dup2(
    oldfd: i32,
    newfd: i32,
    fd_table_guard: &mut RwLockWriteGuard<'_, FileDescriptorVec>,
) -> Result<usize, SystemError> {
    do_dup3(oldfd, newfd, FileMode::empty(), fd_table_guard)
}

pub fn do_dup3(
    oldfd: i32,
    newfd: i32,
    flags: FileMode,
    fd_table_guard: &mut RwLockWriteGuard<'_, FileDescriptorVec>,
) -> Result<usize, SystemError> {
    // 检查 RLIMIT_NOFILE：newfd 必须小于软限制
    let nofile = crate::process::ProcessManager::current_pcb()
        .get_rlimit(crate::process::resource::RLimitID::Nofile)
        .rlim_cur;
    if newfd as u64 >= nofile {
        return Err(SystemError::EMFILE);
    }

    // 确认oldfd, newid是否有效
    if !(fd_table_guard.validate_fd(oldfd) && fd_table_guard.validate_fd(newfd)) {
        return Err(SystemError::EBADF);
    }

    if oldfd == newfd {
        // 若oldfd与newfd相等
        return Ok(newfd as usize);
    }
    let new_exists = fd_table_guard.get_file_by_fd(newfd).is_some();
    if new_exists {
        // close newfd
        if fd_table_guard.drop_fd(newfd).is_err() {
            // An I/O error occurred while attempting to close fildes2.
            return Err(SystemError::EIO);
        }
    }

    let old_file = fd_table_guard
        .get_file_by_fd(oldfd)
        .ok_or(SystemError::EBADF)?;
    let new_file = old_file.try_clone().ok_or(SystemError::EBADF)?;

    if flags.contains(FileMode::O_CLOEXEC) {
        new_file.set_close_on_exec(true);
    } else {
        new_file.set_close_on_exec(false);
    }
    // 申请文件描述符，并把文件对象存入其中
    let res = fd_table_guard
        .alloc_fd(new_file, Some(newfd))
        .map(|x| x as usize);
    return res;
}
