use crate::driver::base::device::Device;

use super::{super::device::DeviceState, CompatibleTable};

/// @brief: 实现该trait的设备实例应挂载在platform总线上，
///         同时应该实现Device trait
pub trait PlatformDevice: Device {
    
    fn compatible_table(&self) -> CompatibleTable;
    /// @brief: 判断设备是否初始化
    /// @parameter: None
    /// @return: 如果已经初始化，返回true，否则，返回false
    fn is_initialized(&self) -> bool;

    /// @brief: 设置设备状态
    /// @parameter set_state: 设备状态
    /// @return: None
    fn set_state(&self, set_state: DeviceState);
}
