use alloc::sync::Arc;
use system_error::SystemError;

use crate::driver::base::device::driver::Driver;

use super::serio_device::SerioDevice;

/// @brief: 实现该trait的设备驱动实例应挂载在serio总线上，
///         同时应该实现Driver trait
/// 参考:  https://code.dragonos.org.cn/xref/linux-6.1.9/include/linux/serio.h#67
pub trait SerioDriver: Driver {

    fn write_wakeup(&self, device: &Arc<dyn SerioDevice>) -> Result<(), SystemError>;
    fn interrupt(&self, device: &Arc<dyn SerioDevice>, char: u8, int: u8) -> Result<(), SystemError>;
    fn connect(&self, device: &Arc<dyn SerioDevice>) -> Result<(), SystemError>;
    fn reconnect(&self, device: &Arc<dyn SerioDevice>) -> Result<(), SystemError>;
    fn fast_reconnect(&self, device: &Arc<dyn SerioDevice>) -> Result<(), SystemError>;
    fn disconnect(&self, device: &Arc<dyn SerioDevice>) -> Result<(), SystemError>;
    fn cleanup(&self, device: &Arc<dyn SerioDevice>) -> Result<(), SystemError>;
}