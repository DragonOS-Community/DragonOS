use system_error::SystemError;

use crate::{
    filesystem::vfs::file::{FileDescriptorVec, FileFlags},
    libs::rwsem::RwSemWriteGuard,
};

pub fn do_dup2(
    oldfd: i32,
    newfd: i32,
    fd_table_guard: &mut RwSemWriteGuard<'_, FileDescriptorVec>,
) -> Result<usize, SystemError> {
    do_dup3(oldfd, newfd, FileFlags::empty(), fd_table_guard)
}

pub fn do_dup3(
    oldfd: i32,
    newfd: i32,
    flags: FileFlags,
    fd_table_guard: &mut RwSemWriteGuard<'_, FileDescriptorVec>,
) -> Result<usize, SystemError> {
    // 检查 RLIMIT_NOFILE：newfd 必须小于软限制（与 Linux ksys_dup3 一致，返回 EBADF）
    let nofile = crate::process::ProcessManager::current_pcb()
        .get_rlimit(crate::process::resource::RLimitID::Nofile)
        .rlim_cur;
    if newfd < 0 || newfd as u64 >= nofile {
        return Err(SystemError::EBADF);
    }

    if oldfd == newfd {
        // dup2(fd, fd) 语义：验证 oldfd 有效后原样返回（不修改 cloexec）
        // 注意：dup3(fd, fd) 的 EINVAL 由 sys_dup3.rs 调用方在调用 do_dup3 之前处理
        fd_table_guard
            .get_file_by_fd(oldfd)
            .ok_or(SystemError::EBADF)?;
        return Ok(newfd as usize);
    }

    // 验证 oldfd 有效（必须在当前 fd 表范围内且已打开）
    // 注意：不需要验证 newfd 的范围，alloc_fd_arc 会自动扩容 fd 表
    // （与 Linux ksys_dup3 中先调用 expand_files(files, newfd) 扩容一致）
    let old_file = fd_table_guard
        .get_file_by_fd(oldfd)
        .ok_or(SystemError::EBADF)?;

    // 如果 newfd 已被占用，先关闭它
    if fd_table_guard.get_file_by_fd(newfd).is_some() && fd_table_guard.drop_fd(newfd).is_err() {
        // An I/O error occurred while attempting to close fildes2.
        return Err(SystemError::EIO);
    }

    let cloexec = flags.contains(FileFlags::O_CLOEXEC);

    // 共享同一个 open file description（Arc<File>），符合 POSIX dup 语义
    let res = fd_table_guard
        .alloc_fd_arc(old_file, Some(newfd), cloexec)
        .map(|x| x as usize);
    return res;
}
