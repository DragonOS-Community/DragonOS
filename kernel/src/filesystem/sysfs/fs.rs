use super::{
    LockedSysFSInode, 
    SYS_FS_INODE
};
use crate::{
    filesystem::vfs::IndexNode, 
    syscall::SystemError
};
use alloc::sync::Arc;

/// @brief: 注册fs，在sys/fs下是生成文件夹
/// @parameter fs_name: 类文件夹名
/// @return: 操作成功，返回inode，操作失败，返回错误码
#[inline]
#[allow(dead_code)]
pub fn fs_register(fs_name: &str) -> Result<Arc<dyn IndexNode>, SystemError> {
    let binding: Arc<dyn IndexNode> = SYS_FS_INODE();
    binding
        .as_any_ref()
        .downcast_ref::<LockedSysFSInode>()
        .ok_or(SystemError::E2BIG)
        .unwrap()
        .add_dir(fs_name)
}

/// @brief: 注销fs，在sys/fs删除文件夹
/// @parameter fs_name: 总线文件夹名
/// @return: 操作成功，返回()，操作失败，返回错误码
#[inline]
#[allow(dead_code)]
pub fn fs_unregister(fs_name: &str) -> Result<(), SystemError> {
    let binding: Arc<dyn IndexNode> = SYS_FS_INODE();
    binding
        .as_any_ref()
        .downcast_ref::<LockedSysFSInode>()
        .ok_or(SystemError::E2BIG)
        .unwrap()
        .remove(fs_name)
}