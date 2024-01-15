use alloc::sync::Arc;
use system_error::SystemError;

use crate::driver::base::device::{
    bus::Bus,
    driver::{driver_manager, Driver},
};

use super::{serio_bus, serio_device::SerioDevice};

/// @brief: 实现该trait的设备驱动实例应挂载在serio总线上，同时应该实现Driver trait
/// 参考:  https://code.dragonos.org.cn/xref/linux-6.1.9/include/linux/serio.h#67
pub trait SerioDriver: Driver {
    // 写入时唤醒设备
    fn write_wakeup(&self, device: &Arc<dyn SerioDevice>) -> Result<(), SystemError>;
    /// @brief: 中断函数
    /// @parameter: 
    /// device: Serio设备
    /// data: 端口数据
    /// flag: 状态掩码
    /// @return: None
    fn interrupt(
        &self,
        device: &Arc<dyn SerioDevice>,
        data: u8,
        flag: u8,
    ) -> Result<(), SystemError>;
    // Serio驱动连接设备
    fn connect(&self, device: &Arc<dyn SerioDevice>) -> Result<(), SystemError>;
    // 重新连接设备
    fn reconnect(&self, device: &Arc<dyn SerioDevice>) -> Result<(), SystemError>;
    // 快速重连设备
    fn fast_reconnect(&self, device: &Arc<dyn SerioDevice>) -> Result<(), SystemError>;
    // 驱动断开设备
    fn disconnect(&self, device: &Arc<dyn SerioDevice>) -> Result<(), SystemError>;
    // 清除设备状态
    fn cleanup(&self, device: &Arc<dyn SerioDevice>) -> Result<(), SystemError>;
}

//todo: https://code.dragonos.org.cn/xref/linux-6.1.9/drivers/input/serio/serio.c#810

pub struct SerioDriverManager;

#[allow(dead_code)]
impl SerioDriverManager {
    /// @brief: 注册Serio驱动
    /// @parameter: 待注册的Serio驱动
    /// @return: None
    pub fn register(&self, driver: Arc<dyn SerioDriver>) -> Result<(), SystemError> {
        driver.set_bus(Some(Arc::downgrade(&(serio_bus() as Arc<dyn Bus>))));
        return driver_manager().register(driver as Arc<dyn Driver>);
    }
    
    /// @brief: 卸载Serio驱动
    /// @parameter: 待卸载的Serio驱动
    /// @return: None
    #[allow(dead_code)]
    pub fn unregister(&self, driver: &Arc<dyn SerioDriver>) {
        driver_manager().unregister(&(driver.clone() as Arc<dyn Driver>));
    }
}
