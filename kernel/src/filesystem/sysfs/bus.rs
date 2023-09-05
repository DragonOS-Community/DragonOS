use super::{LockedSysFSInode, SYS_BUS_INODE};
use crate::{filesystem::vfs::IndexNode, kdebug, syscall::SystemError};
use alloc::sync::Arc;

/// @brief: 注册bus，在sys/bus下生成文件夹
/// @parameter bus_name: 总线文件夹名
/// @return: 操作成功，返回inode，操作失败，返回错误码
#[inline]
#[allow(dead_code)]
pub fn sys_bus_register(bus_name: &str) -> Result<Arc<dyn IndexNode>, SystemError> {
    let binding: Arc<dyn IndexNode> = SYS_BUS_INODE();
    kdebug!("Before bus_register: ls /sys/bus/: {:?}", binding.list());
    binding
        .as_any_ref()
        .downcast_ref::<LockedSysFSInode>()
        .ok_or(SystemError::E2BIG)
        .unwrap()
        .add_dir(bus_name)
}

/// @brief: 注销bus，在sys/bus删除文件夹
/// @parameter bus_name: 总线文件夹名
/// @return: 操作成功，返回()，操作失败，返回错误码
#[allow(dead_code)]
pub fn sys_bus_unregister(bus_name: &str) -> Result<(), SystemError> {
    let binding: Arc<dyn IndexNode> = SYS_BUS_INODE();
    binding
        .as_any_ref()
        .downcast_ref::<LockedSysFSInode>()
        .ok_or(SystemError::E2BIG)
        .unwrap()
        .remove(bus_name)
}

/// @brief: 在相应总线文件夹下生成devices和drivers文件夹
/// @parameter inode: 总线文件夹inode
/// @return: 操作成功，返回devices inode和drivers inode，操作失败，返回错误码
pub fn sys_bus_init(
    inode: &Arc<dyn IndexNode>,
) -> Result<(Arc<dyn IndexNode>, Arc<dyn IndexNode>), SystemError> {
    match inode.as_any_ref().downcast_ref::<LockedSysFSInode>() {
        Some(lock_bus) => match lock_bus.add_dir("devices") {
            Ok(devices) => match lock_bus.add_dir("drivers") {
                Ok(drivers) => Ok((devices, drivers)),
                Err(err) => Err(err),
            },
            Err(err) => Err(err),
        },
        None => Err(SystemError::E2BIG),
    }
}

/// @brief: 在相应总线的device下生成设备文件夹
/// @parameter bus_name: 总线名
///            name: 设备名
/// @return: 操作成功，返回device inode，操作失败，返回错误码
pub fn bus_driver_register(bus_name: &str, name: &str) -> Result<Arc<dyn IndexNode>, SystemError> {
    match SYS_BUS_INODE().find(bus_name) {
        Ok(platform) => match platform.find("drivers") {
            Ok(device) => device
                .as_any_ref()
                .downcast_ref::<LockedSysFSInode>()
                .ok_or(SystemError::E2BIG)
                .unwrap()
                .add_dir(name),
            Err(_) => return Err(SystemError::EXDEV),
        },
        Err(_) => return Err(SystemError::EXDEV),
    }
}

/// @brief: 在相应总线的driver下生成驱动文件夹
/// @parameter bus_name: 总线名
///            name: 驱动名
/// @return: 操作成功，返回drivers inode，操作失败，返回错误码
pub fn bus_device_register(bus_name: &str, name: &str) -> Result<Arc<dyn IndexNode>, SystemError> {
    match SYS_BUS_INODE().find(bus_name) {
        Ok(platform) => match platform.find("devices") {
            Ok(device) => device
                .as_any_ref()
                .downcast_ref::<LockedSysFSInode>()
                .ok_or(SystemError::E2BIG)
                .unwrap()
                .add_dir(name),
            Err(_) => return Err(SystemError::EXDEV),
        },
        Err(_) => return Err(SystemError::EFAULT),
    }
}
