use alloc::sync::Arc;
use system_error::SystemError;

use crate::driver::base::device::{bus::Bus, device_manager, Device};

use super::serio_bus;

/// 参考: https://code.dragonos.org.cn/xref/linux-6.1.9/include/linux/serio.h#20
pub trait SerioDevice: Device {
    fn write(&self, device: &Arc<dyn SerioDevice>, data: u8) -> Result<(), SystemError>;
    fn open(&self, device: &Arc<dyn SerioDevice>) -> Result<(), SystemError>;
    fn close(&self, device: &Arc<dyn SerioDevice>) -> Result<(), SystemError>;
    fn start(&self, device: &Arc<dyn SerioDevice>) -> Result<(), SystemError>;
    fn stop(&self, device: &Arc<dyn SerioDevice>) -> Result<(), SystemError>;
}

#[inline(always)]
pub fn serio_device_manager() -> &'static SerioDeviceManager {
    &SerioDeviceManager
}

pub struct SerioDeviceManager;

impl SerioDeviceManager {
    pub fn register_port(&self, device: Arc<dyn SerioDevice>) -> Result<(), SystemError> {
        self.init_port(device)
    }

    /// todo：https://code.dragonos.org.cn/xref/linux-6.1.9/drivers/input/serio/serio.c#494
    pub fn init_port(&self, device: Arc<dyn SerioDevice>) -> Result<(), SystemError> {
        device.set_bus(Some(Arc::downgrade(&(serio_bus() as Arc<dyn Bus>))));
        device_manager().add_device(device.clone() as Arc<dyn Device>)?;
        Ok(())
    }
}
