use super::Device;
use crate::{driver::Driver, syscall::SystemError};
use alloc::sync::Arc;
use core::fmt::Debug;

/// @brief: Driver error
#[allow(dead_code)]
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum DriverError {
    ProbeError,            // 探测设备失败(该驱动不能初始化这个设备)
    RegisterError,         // 设备注册失败
    AllocateResourceError, // 获取设备所需资源失败
    UnsupportedOperation,  // 不支持的操作
    UnInitialized,         // 未初始化
}

impl Into<SystemError> for DriverError {
    fn into(self) -> SystemError {
        match self {
            DriverError::ProbeError => SystemError::ENODEV,
            DriverError::RegisterError => SystemError::ENODEV,
            DriverError::AllocateResourceError => SystemError::EIO,
            DriverError::UnsupportedOperation => SystemError::EIO,
            DriverError::UnInitialized => SystemError::ENODEV,
        }
    }
}

#[inline(always)]
pub fn driver_manager() -> &'static DriverManager {
    &DriverManager
}

/// @brief: 驱动管理器
#[derive(Debug, Clone)]
pub struct DriverManager;

impl DriverManager {
    /// 参考： https://opengrok.ringotek.cn/xref/linux-6.1.9/drivers/base/dd.c#434
    pub fn driver_sysfs_add(&self, dev: &Arc<dyn Device>) -> Result<(), SystemError> {
        todo!("DriverManager::driver_sysfs_add()");
    }
}

/// @brief: 驱动注册
/// @parameter: name: 驱动名
/// @return: 操作成功，返回()，操作失败，返回错误码
pub fn driver_register(driver: Arc<dyn Driver>) -> Result<(), DriverError> {
    todo!("driver_register()");
}

/// @brief: 驱动卸载
/// @parameter: name: 驱动名
/// @return: 操作成功，返回()，操作失败，返回错误码
#[allow(dead_code)]
pub fn driver_unregister(driver: Arc<dyn Driver>) -> Result<(), DriverError> {
    todo!("driver_unregister()");
}
