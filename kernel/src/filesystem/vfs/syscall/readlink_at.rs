use system_error::SystemError;

use crate::{
    filesystem::vfs::{
        utils::user_path_at, FilePrivateData, FileType, MAX_PATHLEN, VFS_MAX_FOLLOW_SYMLINK_TIMES,
    },
    libs::mutex::Mutex,
    process::ProcessManager,
    syscall::user_access::{check_and_clone_cstr, UserBufferWriter},
};

pub fn do_readlink_at(
    dirfd: i32,
    path: *const u8,
    user_buf: *mut u8,
    buf_size: usize,
) -> Result<usize, SystemError> {
    let path = check_and_clone_cstr(path, Some(MAX_PATHLEN))?
        .into_string()
        .map_err(|_| SystemError::EINVAL)?;
    let path = path.as_str().trim();
    let mut user_buf = UserBufferWriter::new(user_buf, buf_size, true)?;

    let (inode, path) = user_path_at(&ProcessManager::current_pcb(), dirfd, path)?;

    let inode = inode.lookup_follow_symlink2(path.as_str(), VFS_MAX_FOLLOW_SYMLINK_TIMES, false)?;
    if inode.metadata()?.file_type != FileType::SymLink {
        return Err(SystemError::EINVAL);
    }

    let ubuf = user_buf.buffer::<u8>(0).unwrap();

    let len = inode.read_at(
        0,
        buf_size,
        ubuf,
        Mutex::new(FilePrivateData::Unused).lock(),
    )?;

    return Ok(len);
}
