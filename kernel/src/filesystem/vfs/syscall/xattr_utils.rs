use crate::{
    filesystem::vfs::{syscall::AtFlags, utils::user_path_at, IndexNode, XattrFlags, MAX_PATHLEN},
    process::ProcessManager,
    syscall::user_access::{
        check_and_clone_cstr, vfs_check_and_clone_cstr, UserBufferReader, UserBufferWriter,
    },
};
use alloc::{string::String, sync::Arc, vec::Vec};
use system_error::SystemError;

const XATTR_LIST_MAX: usize = 65536;
const XATTR_NAME_MAX: usize = 255;
const XATTR_SIZE_MAX: usize = 65536;

struct SetxattrArgs {
    name: String,
    value: Vec<u8>,
    flags: XattrFlags,
}

fn clone_xattr_name(name_ptr: *const u8) -> Result<String, SystemError> {
    let name = check_and_clone_cstr(name_ptr, Some(XATTR_NAME_MAX + 1))?;
    if name.as_bytes().is_empty() || name.as_bytes().len() > XATTR_NAME_MAX {
        return Err(SystemError::ERANGE);
    }

    name.into_string().map_err(|_| SystemError::EINVAL)
}

fn parse_setxattr_flags(flags: i32) -> Result<XattrFlags, SystemError> {
    XattrFlags::from_bits(flags).ok_or(SystemError::EINVAL)
}

fn prepare_setxattr_args(
    name_ptr: *const u8,
    value_ptr: *const u8,
    size: usize,
    flags: i32,
) -> Result<SetxattrArgs, SystemError> {
    let flags = parse_setxattr_flags(flags)?;
    let name = clone_xattr_name(name_ptr)?;

    if size > XATTR_SIZE_MAX {
        return Err(SystemError::E2BIG);
    }

    let value = if size == 0 {
        Vec::new()
    } else {
        let user_buffer_reader = UserBufferReader::new(value_ptr, size, true)?;
        let mut value = vec![0u8; size];
        user_buffer_reader.copy_from_user_protected(&mut value, 0)?;
        value
    };

    Ok(SetxattrArgs { name, value, flags })
}

/// Extended attribute GET operations
pub(super) fn path_getxattr(
    path_ptr: *const u8,
    name_ptr: *const u8,
    buf_ptr: *mut u8,
    size: usize,
    lookup_flags: usize,
) -> Result<usize, SystemError> {
    let path = vfs_check_and_clone_cstr(path_ptr, Some(MAX_PATHLEN))?
        .into_string()
        .map_err(|_| SystemError::EINVAL)?;

    let pcb = ProcessManager::current_pcb();
    let (current_node, rest_path) = user_path_at(&pcb, AtFlags::AT_FDCWD.bits(), &path)?;
    let inode = current_node.lookup_follow_symlink(&rest_path, lookup_flags)?;

    do_getxattr(inode, name_ptr, buf_ptr, size)
}

pub(super) fn fd_getxattr(
    fd: i32,
    name_ptr: *const u8,
    buf_ptr: *mut u8,
    size: usize,
) -> Result<usize, SystemError> {
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

/// Extended attribute LIST operations
pub(super) fn path_listxattr(
    path_ptr: *const u8,
    buf_ptr: *mut u8,
    size: usize,
    lookup_flags: usize,
) -> Result<usize, SystemError> {
    let path = vfs_check_and_clone_cstr(path_ptr, Some(MAX_PATHLEN))?
        .into_string()
        .map_err(|_| SystemError::EINVAL)?;

    let pcb = ProcessManager::current_pcb();
    let (current_node, rest_path) = user_path_at(&pcb, AtFlags::AT_FDCWD.bits(), &path)?;
    let inode = current_node.lookup_follow_symlink(&rest_path, lookup_flags)?;

    do_listxattr(inode, buf_ptr, size)
}

pub(super) fn fd_listxattr(fd: i32, buf_ptr: *mut u8, size: usize) -> Result<usize, SystemError> {
    let binding = ProcessManager::current_pcb().fd_table();
    let fd_table_guard = binding.read();

    let file = fd_table_guard
        .get_file_by_fd(fd)
        .ok_or(SystemError::EBADF)?;
    let inode = file.inode();

    do_listxattr(inode, buf_ptr, size)
}

fn do_listxattr(
    inode: Arc<dyn IndexNode>,
    buf_ptr: *mut u8,
    size: usize,
) -> Result<usize, SystemError> {
    if size == 0 {
        let mut temp_buf = Vec::new();
        return inode.listxattr(&mut temp_buf);
    }

    let capped_size = core::cmp::min(size, XATTR_LIST_MAX);
    let mut list = vec![0u8; capped_size];
    let actual_size = match inode.listxattr(&mut list) {
        Err(SystemError::ERANGE) if capped_size == XATTR_LIST_MAX => {
            return Err(SystemError::E2BIG)
        }
        result => result?,
    };
    if actual_size > capped_size {
        if capped_size == XATTR_LIST_MAX {
            return Err(SystemError::E2BIG);
        }
        return Err(SystemError::ERANGE);
    }

    if actual_size == 0 {
        return Ok(0);
    }

    let mut user_buffer_writer = UserBufferWriter::new(buf_ptr, actual_size, true)?;
    user_buffer_writer.copy_to_user(&list[..actual_size], 0)?;
    Ok(actual_size)
}

/// Extended attribute REMOVE operations
pub(super) fn path_removexattr(
    path_ptr: *const u8,
    name_ptr: *const u8,
    lookup_flags: usize,
) -> Result<usize, SystemError> {
    let path = vfs_check_and_clone_cstr(path_ptr, Some(MAX_PATHLEN))?
        .into_string()
        .map_err(|_| SystemError::EINVAL)?;

    let pcb = ProcessManager::current_pcb();
    let (current_node, rest_path) = user_path_at(&pcb, AtFlags::AT_FDCWD.bits(), &path)?;
    let inode = current_node.lookup_follow_symlink(&rest_path, lookup_flags)?;

    do_removexattr(inode, name_ptr)
}

pub(super) fn fd_removexattr(fd: i32, name_ptr: *const u8) -> Result<usize, SystemError> {
    let binding = ProcessManager::current_pcb().fd_table();
    let fd_table_guard = binding.read();

    let file = fd_table_guard
        .get_file_by_fd(fd)
        .ok_or(SystemError::EBADF)?;
    let inode = file.inode();

    do_removexattr(inode, name_ptr)
}

fn do_removexattr(inode: Arc<dyn IndexNode>, name_ptr: *const u8) -> Result<usize, SystemError> {
    let name = clone_xattr_name(name_ptr)?;

    inode.removexattr(&name)
}

fn do_getxattr(
    inode: Arc<dyn IndexNode>,
    name_ptr: *const u8,
    buf_ptr: *mut u8,
    size: usize,
) -> Result<usize, SystemError> {
    let name = clone_xattr_name(name_ptr)?;

    if size == 0 {
        // 只返回需要的缓冲区大小
        let mut temp_buf = Vec::new();
        let result_size = inode.getxattr(&name, &mut temp_buf)?;
        Ok(result_size)
    } else {
        let capped_size = core::cmp::min(size, XATTR_SIZE_MAX);
        let mut value = vec![0u8; capped_size];
        let actual_size = match inode.getxattr(&name, &mut value) {
            Err(SystemError::ERANGE) if capped_size == XATTR_SIZE_MAX => {
                return Err(SystemError::E2BIG)
            }
            result => result?,
        };
        if actual_size > capped_size {
            if capped_size == XATTR_SIZE_MAX {
                return Err(SystemError::E2BIG);
            }
            return Err(SystemError::ERANGE);
        }
        if actual_size == 0 {
            return Ok(0);
        }

        let mut user_buffer_writer = UserBufferWriter::new(buf_ptr, actual_size, true)?;
        user_buffer_writer.copy_to_user(&value[..actual_size], 0)?;
        Ok(actual_size)
    }
}

/// Extended attribute SET operations
pub(super) fn path_setxattr(
    path_ptr: *const u8,
    name_ptr: *const u8,
    value_ptr: *const u8,
    size: usize,
    lookup_flags: usize,
    flags: i32,
) -> Result<usize, SystemError> {
    let xattr = prepare_setxattr_args(name_ptr, value_ptr, size, flags)?;

    let path = vfs_check_and_clone_cstr(path_ptr, Some(MAX_PATHLEN))?
        .into_string()
        .map_err(|_| SystemError::EINVAL)?;

    let pcb = ProcessManager::current_pcb();
    let (current_node, rest_path) = user_path_at(&pcb, AtFlags::AT_FDCWD.bits(), &path)?;
    let inode = current_node.lookup_follow_symlink(&rest_path, lookup_flags)?;

    do_setxattr(inode, xattr)
}

pub(super) fn fd_setxattr(
    fd: i32,
    name_ptr: *const u8,
    value_ptr: *const u8,
    size: usize,
    flags: i32,
) -> Result<usize, SystemError> {
    let binding = ProcessManager::current_pcb().fd_table();
    let fd_table_guard = binding.read();

    let file = fd_table_guard
        .get_file_by_fd(fd)
        .ok_or(SystemError::EBADF)?;
    let inode = file.inode();

    let xattr = prepare_setxattr_args(name_ptr, value_ptr, size, flags)?;
    do_setxattr(inode, xattr)
}

fn do_setxattr(inode: Arc<dyn IndexNode>, xattr: SetxattrArgs) -> Result<usize, SystemError> {
    inode.setxattr(&xattr.name, &xattr.value, xattr.flags)
}
