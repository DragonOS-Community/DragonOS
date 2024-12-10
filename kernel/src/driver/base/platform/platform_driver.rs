use alloc::sync::Arc;

use crate::driver::base::device::{
    bus::Bus,
    driver::{driver_manager, Driver},
};

use system_error::SystemError;

use super::{platform_bus, platform_device::PlatformDevice};

/// @brief: 实现该trait的设备驱动实例应挂载在platform总线上，
///         同时应该实现Driver trait
///
/// ## 注意
///
/// 应当在所有实现这个trait的结构体上方，添加 `#[cast_to([sync] PlatformDriver)]`，
/// 否则运行时将报错“该对象不是PlatformDriver”
#[allow(dead_code)]
pub trait PlatformDriver: Driver {
    /// 检测设备是否能绑定到这个驱动
    ///
    /// 如果能，则把设备的driver字段指向这个驱动。
    /// 请注意，这个函数不应该把driver加入驱动的devices列表，相关工作会在外部的函数里面处理。
    fn probe(&self, device: &Arc<dyn PlatformDevice>) -> Result<(), SystemError>;
    fn remove(&self, device: &Arc<dyn PlatformDevice>) -> Result<(), SystemError>;
    fn shutdown(&self, device: &Arc<dyn PlatformDevice>) -> Result<(), SystemError>;
    fn suspend(&self, device: &Arc<dyn PlatformDevice>) -> Result<(), SystemError>;
    fn resume(&self, device: &Arc<dyn PlatformDevice>) -> Result<(), SystemError>;
}

#[inline(always)]
pub fn platform_driver_manager() -> &'static PlatformDriverManager {
    &PlatformDriverManager
}

#[derive(Debug)]
pub struct PlatformDriverManager;

impl PlatformDriverManager {
    /// 注册平台设备驱动
    ///
    /// 参考 https://code.dragonos.org.cn/xref/linux-6.1.9/drivers/base/platform.c?fi=__platform_driver_register#861
    pub fn register(&self, driver: Arc<dyn PlatformDriver>) -> Result<(), SystemError> {
        driver.set_bus(Some(Arc::downgrade(&(platform_bus() as Arc<dyn Bus>))));
        return driver_manager().register(driver as Arc<dyn Driver>);
    }

    /// 卸载平台设备驱动
    #[allow(dead_code)]
    pub fn unregister(&self, driver: &Arc<dyn PlatformDriver>) {
        driver_manager().unregister(&(driver.clone() as Arc<dyn Driver>));
    }
}
