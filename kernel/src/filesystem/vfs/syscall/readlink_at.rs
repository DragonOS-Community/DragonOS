use alloc::vec::Vec;
use system_error::SystemError;

use crate::{
    filesystem::vfs::{
        fcntl::AtFlags, utils::user_path_at, FilePrivateData, FileType, MAX_PATHLEN,
        VFS_MAX_FOLLOW_SYMLINK_TIMES,
    },
    libs::mutex::Mutex,
    mm::VirtAddr,
    process::ProcessManager,
    syscall::user_access::{check_and_clone_cstr, copy_to_user_protected},
};

pub fn do_readlink_at(
    dirfd: i32,
    path: *const u8,
    user_buf: *mut u8,
    buf_size: usize,
) -> Result<usize, SystemError> {
    if buf_size == 0 || buf_size > i32::MAX as usize {
        return Err(SystemError::EINVAL);
    }

    let path = check_and_clone_cstr(path, Some(MAX_PATHLEN))?
        .into_string()
        .map_err(|_| SystemError::EINVAL)?;
    let (inode, _file_guard) = if path.is_empty() {
        if dirfd == AtFlags::AT_FDCWD.bits() {
            return Err(SystemError::ENOENT);
        }

        let file = ProcessManager::current_pcb()
            .fd_table()
            .read()
            .get_file_by_fd(dirfd)
            .ok_or(SystemError::EBADF)?;
        // Retain the open file description (and its mount/inode pins) until
        // after the symlink contents have been read and copied to userspace.
        (file.path_inode(), Some(file))
    } else {
        let (start, path) = user_path_at(&ProcessManager::current_pcb(), dirfd, &path)?;
        (
            start.lookup_follow_symlink2(&path, VFS_MAX_FOLLOW_SYMLINK_TIMES, false)?,
            None,
        )
    };
    if inode.metadata()?.file_type != FileType::SymLink {
        return Err(if path.is_empty() {
            SystemError::ENOENT
        } else {
            SystemError::EINVAL
        });
    }

    // Filesystem read_at methods expect a kernel slice. Writing directly into
    // a userspace slice is not protected against page faults or concurrent
    // unmapping; copy out only the actual result through the exception table.
    let mut capacity = buf_size.min(MAX_PATHLEN);
    let mut buffer = Vec::new();
    buffer
        .try_reserve_exact(capacity)
        .map_err(|_| SystemError::ENOMEM)?;
    buffer.resize(capacity, 0);
    let private_data = Mutex::new(FilePrivateData::Unused);
    loop {
        let len = inode.read_at(0, capacity, &mut buffer, private_data.lock())?;
        if len > capacity {
            return Err(SystemError::EIO);
        }
        if len < capacity || capacity == buf_size {
            // SAFETY: copy_to_user_protected validates the userspace range and
            // handles faults through the exception table.
            unsafe { copy_to_user_protected(VirtAddr::new(user_buf as usize), &buffer[..len])? };
            return Ok(len);
        }

        let next_capacity = capacity.saturating_mul(2).min(buf_size);
        buffer
            .try_reserve_exact(next_capacity - capacity)
            .map_err(|_| SystemError::ENOMEM)?;
        buffer.resize(next_capacity, 0);
        capacity = next_capacity;
    }
}
