use super::{LockedSysFSInode, SYS_CLASS_INODE};
use crate::{filesystem::vfs::IndexNode, syscall::SystemError};
use alloc::sync::Arc;

/// @brief: 注册class，在sys/class下生成文件夹
/// @parameter class_name: 类文件夹名
/// @return: 操作成功，返回inode，操作失败，返回错误码
#[inline]
#[allow(dead_code)]
pub fn sys_class_register(class_name: &str) -> Result<Arc<dyn IndexNode>, SystemError> {
    let binding: Arc<dyn IndexNode> = SYS_CLASS_INODE();
    binding
        .as_any_ref()
        .downcast_ref::<LockedSysFSInode>()
        .ok_or(SystemError::E2BIG)
        .unwrap()
        .add_dir(class_name)
}

/// @brief: 注销class，在sys/class删除文件夹
/// @parameter class_name: 总线文件夹名
/// @return: 操作成功，返回()，操作失败，返回错误码
#[inline]
#[allow(dead_code)]
pub fn sys_class_unregister(class_name: &str) -> Result<(), SystemError> {
    let binding: Arc<dyn IndexNode> = SYS_CLASS_INODE();
    binding
        .as_any_ref()
        .downcast_ref::<LockedSysFSInode>()
        .ok_or(SystemError::E2BIG)
        .unwrap()
        .remove(class_name)
}

/// @brief: 注册device，在对应类下操作设备文件夹
/// @parameter class: 类文件夹inode
/// @parameter device_name: 设备文件夹名
/// @return: 操作成功，返回inode，操作失败，返回错误码
#[inline]
#[allow(dead_code)]
pub fn class_device_register(
    class: Arc<dyn IndexNode>,
    device_name: &str,
) -> Result<Arc<dyn IndexNode>, SystemError> {
    class
        .as_any_ref()
        .downcast_ref::<LockedSysFSInode>()
        .ok_or(SystemError::E2BIG)
        .unwrap()
        .add_dir(device_name)
}

/// @brief: 操作device，在对应类下删除设备文件夹
/// @parameter class: 类文件夹inode
/// @parameter device_name: 设备文件夹名
/// @return: 操作成功，返回()，操作失败，返回错误码
#[inline]
#[allow(dead_code)]
pub fn class_device_unregister(class: Arc<dyn IndexNode>, device_name: &str) -> Result<(), SystemError> {
    class
        .as_any_ref()
        .downcast_ref::<LockedSysFSInode>()
        .ok_or(SystemError::E2BIG)
        .unwrap()
        .remove(device_name)
}
