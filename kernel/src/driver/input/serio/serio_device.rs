use alloc::sync::Arc;
use system_error::SystemError;

use crate::driver::base::device::{bus::Bus, Device};

use super::serio_bus;

/// @brief: 串行设备，实现该trait的设备实例挂载在serio总线上，同时应该实现Device trait
///
/// 参考: https://code.dragonos.org.cn/xref/linux-6.1.9/include/linux/serio.h#20
pub trait SerioDevice: Device {
    /// @brief: Serio设备写入数据
    /// @parametrr: data 数据
    /// @return: None
    fn write(&self, device: &Arc<dyn SerioDevice>, data: u8) -> Result<(), SystemError>;
    // Serio设备连接驱动时调用
    fn open(&self, device: &Arc<dyn SerioDevice>) -> Result<(), SystemError>;
    // Serio设备断开驱动时调用
    fn close(&self, device: &Arc<dyn SerioDevice>) -> Result<(), SystemError>;
    // Serio设备初始化时调用
    fn start(&self, device: &Arc<dyn SerioDevice>) -> Result<(), SystemError>;
    // Serio设备销毁时调用
    fn stop(&self, device: &Arc<dyn SerioDevice>) -> Result<(), SystemError>;
}

#[allow(dead_code)]
#[inline(always)]
pub fn serio_device_manager() -> &'static SerioDeviceManager {
    &SerioDeviceManager
}

pub struct SerioDeviceManager;

#[allow(dead_code)]
impl SerioDeviceManager {
    /// @brief: 注册Serio设备
    /// @parameter: device 待注册的设备
    /// @return: None
    pub fn register_port(&self, device: Arc<dyn SerioDevice>) -> Result<(), SystemError> {
        self.init_port(device)
    }

    /// @brief: 初始化Serio设备
    /// @parameter: device 待初始化的Serio设备
    /// @return: None
    //todo：https://code.dragonos.org.cn/xref/linux-6.1.9/drivers/input/serio/serio.c#494
    pub fn init_port(&self, device: Arc<dyn SerioDevice>) -> Result<(), SystemError> {
        device.set_bus(Some(Arc::downgrade(&(serio_bus() as Arc<dyn Bus>))));
        Ok(())
    }
}
