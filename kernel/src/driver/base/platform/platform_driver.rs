use crate::driver::{base::device::DevicePrivateData, Driver};

use super::{super::device::driver::DriverError, platform_device::PlatformDevice, CompatibleTable};

lazy_static! {
    static ref PLATFORM_COMPAT_TABLE: CompatibleTable = CompatibleTable::new(vec!["platform"]);
}
/// @brief: 实现该trait的设备驱动实例应挂载在platform总线上，
///         同时应该实现Driver trait
pub trait PlatformDriver: Driver {
    fn compatible_table(&self) -> CompatibleTable;
    /// @brief 探测设备
    /// @param data 设备初始拥有的基本信息
    fn probe(&self, data: DevicePrivateData) -> Result<(), DriverError> {
        if data.compatible_table().matches(&PLATFORM_COMPAT_TABLE) {
            return Ok(());
        } else {
            return Err(DriverError::UnsupportedOperation);
        }
    }
}
