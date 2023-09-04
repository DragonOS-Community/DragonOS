use crate::driver::{base::device::DevicePrivateData, Driver};

use super::{super::device::driver::DriverError, CompatibleTable};

/// @brief: 实现该trait的设备驱动实例应挂载在platform总线上，
///         同时应该实现Driver trait
pub trait PlatformDriver: Driver {
    
    fn compatible_table(&self) -> CompatibleTable;
    /// @brief 探测设备
    /// @param data 设备初始拥有的基本信息
    fn probe(&self, data: DevicePrivateData) -> Result<(), DriverError> {
        let platform_list = vec!["platform"];
        if data
            .compatible_table()
            .matches(&CompatibleTable::new(platform_list))
        {
            return Ok(());
        } else {
            return Err(DriverError::UnsupportedOperation);
        }
    }
}
