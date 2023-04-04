use alloc::sync::Arc;
use super::{
    super::device::driver::*,
    platform_device::PlatformDevice,
    CompatibleTable,
};

#[allow(dead_code)]
#[derive(Debug)]
#[derive(PartialEq, Eq)]
#[derive(Clone, Copy)]
pub enum DriverError {
    ProbeError,
}

/// @brief: 实现该trait的设备驱动实例应挂载在platform总线上，
///         同时应该实现Driver trait
pub trait PlatformDriver: Driver {
    /// @brief: 设备驱动探测函数，此函数在设备和驱动匹配成功后调用
    /// @parameter device: 匹配成功的设备实例
    /// @return: 成功驱动设备，返回Ok(())，否则，返回DriverError
    fn probe(&self, device: Arc<dyn PlatformDevice>) -> Result<(), DriverError>;

    /// @brief: 获取驱动匹配表
    /// @parameter: None
    /// @return: 驱动匹配表
    fn get_compatible_table(&self) -> CompatibleTable;
}
