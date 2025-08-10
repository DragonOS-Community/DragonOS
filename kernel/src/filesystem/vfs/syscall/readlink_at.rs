use system_error::SystemError;

use crate::{
    filesystem::vfs::{
        FileType, MAX_PATHLEN,
        file::{File, FileMode},
        utils::user_path_at,
    },
    process::ProcessManager,
    syscall::user_access::{UserBufferWriter, check_and_clone_cstr},
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

    let inode = inode.lookup(path.as_str())?;
    if inode.metadata()?.file_type != FileType::SymLink {
        return Err(SystemError::EINVAL);
    }

    let ubuf = user_buf.buffer::<u8>(0).unwrap();

    let file = File::new(inode, FileMode::O_RDONLY)?;

    let len = file.read(buf_size, ubuf)?;

    return Ok(len);
}
