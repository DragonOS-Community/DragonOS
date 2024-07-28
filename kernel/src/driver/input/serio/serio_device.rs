use alloc::sync::Arc;
use system_error::SystemError;

use crate::driver::base::device::{bus::Bus, device_manager, Device};

use super::serio_bus;

/// 串行设备，实现该trait的设备实例挂载在serio总线上，同时应该实现Device trait
///
/// 参考: https://code.dragonos.org.cn/xref/linux-6.1.9/include/linux/serio.h#20
#[allow(dead_code)]
pub trait SerioDevice: Device {
    /// # 函数功能
    ///
    /// Serio设备写入数据
    ///
    /// ## 参数
    ///
    /// - data 写入的数据
    ///
    /// ## 返回值
    ///
    /// 无
    fn write(&self, device: &Arc<dyn SerioDevice>, data: u8) -> Result<(), SystemError>;
    /// Serio设备连接驱动时调用
    fn open(&self, device: &Arc<dyn SerioDevice>) -> Result<(), SystemError>;
    /// Serio设备断开驱动时调用
    fn close(&self, device: &Arc<dyn SerioDevice>) -> Result<(), SystemError>;
    /// Serio设备初始化时调用
    fn start(&self, device: &Arc<dyn SerioDevice>) -> Result<(), SystemError>;
    /// Serio设备销毁时调用
    fn stop(&self, device: &Arc<dyn SerioDevice>) -> Result<(), SystemError>;
}

#[inline(always)]
pub fn serio_device_manager() -> &'static SerioDeviceManager {
    &SerioDeviceManager
}

pub struct SerioDeviceManager;

impl SerioDeviceManager {
    /// # 函数功能
    /// 注册Serio设备
    ///
    /// ## 参数
    /// - device 待注册的设备
    ///
    /// ## 返回值
    /// 无
    pub fn register_port(&self, device: Arc<dyn SerioDevice>) -> Result<(), SystemError> {
        self.init_port(device)
    }

    /// # 函数功能
    /// 初始化Serio设备
    ///
    /// ## 参数
    /// - device 待初始化的Serio设备
    ///
    /// ## 返回值
    /// 无
    ///
    /// todo：https://code.dragonos.org.cn/xref/linux-6.1.9/drivers/input/serio/serio.c#494
    pub fn init_port(&self, device: Arc<dyn SerioDevice>) -> Result<(), SystemError> {
        device.set_bus(Some(Arc::downgrade(&(serio_bus() as Arc<dyn Bus>))));
        device_manager().add_device(device.clone() as Arc<dyn Device>)?;
        Ok(())
    }
}
