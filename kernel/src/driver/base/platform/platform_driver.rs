use crate::driver::{base::device::DevicePrivateData, Driver};

use super::super::device::driver::DriverError;

/// @brief: 实现该trait的设备驱动实例应挂载在platform总线上，
///         同时应该实现Driver trait
pub trait PlatformDriver: Driver {
    /// @brief 探测设备
    /// @param data 设备初始拥有的基本信息
    fn probe(&self, data: DevicePrivateData) -> Result<(), DriverError> {
        if let Some(compatible_table) = data.compatible_table() {
            if compatible_table.0.contains("platform") {
                return Ok(());
            }
        }
        return Err(DriverError::UnsupportedOperation);
    }
}
