use super::{XATTR_CREATE, XATTR_REPLACE};
use crate::{
    filesystem::vfs::{IndexNode, utils::user_path_at, MAX_PATHLEN, syscall::AtFlags},
    process::ProcessManager,
    syscall::user_access::{check_and_clone_cstr, UserBufferReader, UserBufferWriter},
};
use alloc::{sync::Arc, vec::Vec};
use system_error::SystemError;

/// Extended attribute GET operations

pub(super) fn path_getxattr(path_ptr: *const u8, name_ptr: *const u8, buf_ptr: *mut u8, size: usize, lookup_flags: usize) -> Result<usize, SystemError> {
    let path = check_and_clone_cstr(path_ptr, Some(MAX_PATHLEN))?
        .into_string()
        .map_err(|_| SystemError::EINVAL)?;

    let pcb = ProcessManager::current_pcb();
    let (current_node, rest_path) =
        user_path_at(&pcb, AtFlags::AT_FDCWD.bits(), &path)?;
    let inode = current_node.lookup_follow_symlink(&rest_path, lookup_flags)?;

    do_getxattr(inode, name_ptr, buf_ptr, size)
}

pub(super) fn fd_getxattr(fd: i32, name_ptr: *const u8, buf_ptr: *mut u8, size: usize) -> Result<usize, SystemError> {
    // 获取文件描述符对应的文件节点
    let binding = ProcessManager::current_pcb().fd_table();
    let fd_table_guard = binding.read();

    let file = fd_table_guard
        .get_file_by_fd(fd)
        .ok_or(SystemError::EBADF)?;
    let inode = file.inode();

    // 调用VFS接口获取扩展属性
    do_getxattr(inode, name_ptr, buf_ptr, size)
}

fn do_getxattr(inode: Arc<dyn IndexNode>, name_ptr: *const u8, buf_ptr: *mut u8, size: usize) -> Result<usize, SystemError> {
    let name = check_and_clone_cstr(name_ptr, None)?
        .into_string()
        .map_err(|_| SystemError::EINVAL)?;

    if size == 0 {
        // 只返回需要的缓冲区大小
        let mut temp_buf = Vec::new();
        let result_size = inode.getxattr(&name, &mut temp_buf)?;
        Ok(result_size)
    } else {
        let mut user_buffer_writer = UserBufferWriter::new(buf_ptr, size, true)?;
        let user_buf = user_buffer_writer.buffer(0)?;

        // 读取属性值
        let actual_size = inode.getxattr(&name, user_buf)?;
        Ok(actual_size)
    }
}

/// Extended attribute SET operations

pub(super) fn path_setxattr(path_ptr: *const u8, name_ptr: *const u8, value_ptr: *const u8, size: usize, lookup_flags: usize, flags: i32) -> Result<usize, SystemError> {
    let path = check_and_clone_cstr(path_ptr, Some(MAX_PATHLEN))?
        .into_string()
        .map_err(|_| SystemError::EINVAL)?;

    let pcb = ProcessManager::current_pcb();
    let (current_node, rest_path) =
        user_path_at(&pcb, AtFlags::AT_FDCWD.bits(), &path)?;
    let inode = current_node.lookup_follow_symlink(&rest_path, lookup_flags)?;

    do_setxattr(inode, name_ptr, value_ptr, size, flags)
}

pub(super) fn fd_setxattr(fd: i32, name_ptr: *const u8, value_ptr: *const u8, size: usize, flags: i32) -> Result<usize, SystemError> {
    let binding = ProcessManager::current_pcb().fd_table();
    let fd_table_guard = binding.read();

    let file = fd_table_guard
        .get_file_by_fd(fd)
        .ok_or(SystemError::EBADF)?;
    let inode = file.inode();

    do_setxattr(inode, name_ptr, value_ptr, size, flags)
}

fn do_setxattr(inode: Arc<dyn IndexNode>, name_ptr: *const u8, value_ptr: *const u8, size: usize, flags: i32) -> Result<usize, SystemError> {
    let name = check_and_clone_cstr(name_ptr, None)?
        .into_string()
        .map_err(|_| SystemError::EINVAL)?;
    
    if (flags & XATTR_CREATE != 0) && inode.getxattr(&name, &mut Vec::new()).is_ok() {
        return Err(SystemError::EEXIST);
    }
    if (flags & XATTR_REPLACE != 0) && inode.getxattr(&name, &mut Vec::new()).is_err() {
        return Err(SystemError::ENODATA);
    }
    
    let user_buffer_reader = UserBufferReader::new(value_ptr, size, true)?;
    let value_buf = user_buffer_reader.buffer(0)?; 

    inode.setxattr(&name, value_buf)
}
    