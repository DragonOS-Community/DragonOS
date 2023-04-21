use super::{
    LockedSysFSInode, 
    SYS_DEVICES_INODE
};
use crate::{
    filesystem::vfs::IndexNode, 
    syscall::SystemError
};
use alloc::sync::Arc;

/// @brief: 注册device，在sys/devices下生成文件夹
/// @parameter device_name: 类文件夹名
/// @return: 操作成功，返回inode，操作失败，返回错误码
#[inline]
#[allow(dead_code)]
pub fn device_register(device_name: &str) -> Result<Arc<dyn IndexNode>, SystemError> {
    let binding: Arc<dyn IndexNode> = SYS_DEVICES_INODE();
    binding
        .as_any_ref()
        .downcast_ref::<LockedSysFSInode>()
        .ok_or(SystemError::E2BIG)
        .unwrap()
        .add_dir(device_name)
}

/// @brief: 操作bus，在sys/devices删除文件夹
/// @parameter device_name: 总线文件夹名
/// @return: 操作成功，返回()，操作失败，返回错误码
#[inline]
#[allow(dead_code)]
pub fn device_unregister(device_name: &str) -> Result<(), SystemError> {
    let binding: Arc<dyn IndexNode> = SYS_DEVICES_INODE();
    binding
        .as_any_ref()
        .downcast_ref::<LockedSysFSInode>()
        .ok_or(SystemError::E2BIG)
        .unwrap()
        .remove(device_name)
}