use system_error::SystemError;

use crate::filesystem::vfs::{
    fdtable::{DroppedFd, FileDescriptorTable},
    file::FileFlags,
};

pub fn do_dup2(
    oldfd: i32,
    newfd: i32,
    fd_table: &FileDescriptorTable,
    soft_limit: usize,
) -> Result<(usize, Option<DroppedFd>), SystemError> {
    if oldfd == newfd {
        fd_table.get_file_by_fd(oldfd).ok_or(SystemError::EBADF)?;
        return Ok((newfd as usize, None));
    }
    do_dup3(oldfd, newfd, FileFlags::empty(), fd_table, soft_limit)
}

pub fn do_dup3(
    oldfd: i32,
    newfd: i32,
    flags: FileFlags,
    fd_table: &FileDescriptorTable,
    soft_limit: usize,
) -> Result<(usize, Option<DroppedFd>), SystemError> {
    if newfd < 0 || newfd as usize >= soft_limit {
        return Err(SystemError::EBADF);
    }

    let cloexec = flags.contains(FileFlags::O_CLOEXEC);
    let (res, dropped) = fd_table.duplicate_exact(oldfd, newfd, cloexec, soft_limit)?;
    return Ok((res as usize, dropped));
}
