use crate::{
    filesystem::vfs::{IndexNode, utils::user_path_at, MAX_PATHLEN, syscall::AtFlags},
    process::ProcessManager,
    syscall::user_access::check_and_clone_cstr,
};
use alloc::{sync::Arc, vec::Vec};
use system_error::SystemError;

pub(super) fn path_getxattr(path_ptr: *const u8, name_ptr: *const u8, user_buf: &mut [u8], size: usize, lookup_flags: usize) -> Result<usize, SystemError> {
    let path = check_and_clone_cstr(path_ptr, Some(MAX_PATHLEN))?
        .into_string()
        .map_err(|_| SystemError::EINVAL)?;

    let pcb = ProcessManager::current_pcb();
    let (current_node, rest_path) =
        user_path_at(&pcb, AtFlags::AT_FDCWD.bits(), &path)?;
    let inode = current_node.lookup_follow_symlink(&rest_path, lookup_flags)?;

    do_getxattr(inode, name_ptr, user_buf, size)
}

pub(super) fn fd_getxattr(fd: i32, name_ptr: *const u8, user_buf: &mut [u8], size: usize) -> Result<usize, SystemError> {
    // 获取文件描述符对应的文件节点
    let binding = ProcessManager::current_pcb().fd_table();
    let fd_table_guard = binding.read();

    let file = fd_table_guard
        .get_file_by_fd(fd)
        .ok_or(SystemError::EBADF)?;
    let inode = file.inode();

    // 调用VFS接口获取扩展属性
    do_getxattr(inode, name_ptr, user_buf, size)
}

fn do_getxattr(inode: Arc<dyn IndexNode>, name_ptr: *const u8, user_buf: &mut [u8], size: usize) -> Result<usize, SystemError> {
    let name = check_and_clone_cstr(name_ptr, None)?
        .into_string()
        .map_err(|_| SystemError::EINVAL)?;
    
    if size == 0 {
        // 只返回需要的缓冲区大小
        let mut temp_buf = Vec::new();
        let result_size = inode.getxattr(&name, &mut temp_buf)?;
        Ok(result_size)
    } else {
        // 读取属性值
        let actual_size = inode.getxattr(&name, user_buf)?;
        Ok(actual_size)
    }
}