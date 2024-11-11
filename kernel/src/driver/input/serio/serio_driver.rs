use alloc::sync::Arc;
use system_error::SystemError;

use crate::driver::base::device::{
    bus::Bus,
    driver::{driver_manager, Driver},
};

use super::{serio_bus, serio_device::SerioDevice};

/// 实现该trait的设备驱动实例应挂载在serio总线上，同时应该实现Driver trait
///
/// 参考:  https://code.dragonos.org.cn/xref/linux-6.1.9/include/linux/serio.h#67
#[allow(dead_code)]
pub trait SerioDriver: Driver {
    // 写入时唤醒设备
    fn write_wakeup(&self, device: &Arc<dyn SerioDevice>) -> Result<(), SystemError>;
    /// # 函数功能
    /// 中断函数
    ///
    /// ## 参数
    /// - device: Serio设备
    /// - data: 端口数据
    /// - flag: 状态掩码
    ///
    /// ## 返回值
    /// 无
    ///
    /// todo:https://code.dragonos.org.cn/xref/linux-6.1.9/drivers/input/serio/serio.c?fi=__serio_register_driver#989
    fn interrupt(
        &self,
        device: &Arc<dyn SerioDevice>,
        data: u8,
        flag: u8,
    ) -> Result<(), SystemError>;
    /// Serio驱动连接设备
    fn connect(&self, device: &Arc<dyn SerioDevice>) -> Result<(), SystemError>;
    /// 重新连接设备
    fn reconnect(&self, device: &Arc<dyn SerioDevice>) -> Result<(), SystemError>;
    /// 快速重连设备
    fn fast_reconnect(&self, device: &Arc<dyn SerioDevice>) -> Result<(), SystemError>;
    /// 驱动断开设备
    fn disconnect(&self, device: &Arc<dyn SerioDevice>) -> Result<(), SystemError>;
    /// 清除设备状态
    fn cleanup(&self, device: &Arc<dyn SerioDevice>) -> Result<(), SystemError>;
}

/// todo: https://code.dragonos.org.cn/xref/linux-6.1.9/drivers/input/serio/serio.c#810
#[allow(dead_code)]
#[inline(always)]
pub fn serio_driver_manager() -> &'static SerioDriverManager {
    &SerioDriverManager
}

pub struct SerioDriverManager;

#[allow(dead_code)]
impl SerioDriverManager {
    /// # 函数功能
    /// 注册Serio驱动
    ///
    /// ## 参数
    /// - driver 待注册的Serio驱动
    ///
    /// ## 返回值
    /// 无
    pub fn register(&self, driver: Arc<dyn SerioDriver>) -> Result<(), SystemError> {
        driver.set_bus(Some(Arc::downgrade(&(serio_bus() as Arc<dyn Bus>))));
        return driver_manager().register(driver as Arc<dyn Driver>);
    }

    /// # 函数功能
    /// 卸载Serio驱动
    ///
    /// ## 参数
    /// - driver 待卸载的Serio驱动
    ///
    /// ## 返回值
    /// 无
    #[allow(dead_code)]
    pub fn unregister(&self, driver: &Arc<dyn SerioDriver>) {
        driver_manager().unregister(&(driver.clone() as Arc<dyn Driver>));
    }
}
