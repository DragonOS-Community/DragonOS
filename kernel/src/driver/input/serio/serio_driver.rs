use alloc::sync::Arc;
use system_error::SystemError;

use crate::driver::base::device::{driver::{Driver, driver_manager}, bus::Bus};

use super::{serio_device::SerioDevice, serio_bus};

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

//todo: https://code.dragonos.org.cn/xref/linux-6.1.9/drivers/input/serio/serio.c#810

pub struct SerioDriverManager;

impl SerioDriverManager {

    pub fn register(&self, driver: Arc<dyn SerioDriver>) -> Result<(), SystemError> {
        driver.set_bus(Some(serio_bus() as Arc<dyn Bus>));
        return driver_manager().register(driver as Arc<dyn Driver>);
    }        

    #[allow(dead_code)]
    pub fn unregister(&self, driver: &Arc<dyn SerioDriver>) {
        driver_manager().unregister(&(driver.clone() as Arc<dyn Driver>));
    }
}