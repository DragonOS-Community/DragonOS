use alloc::sync::Arc;

use crate::{
    driver::{base::device::DevicePrivateData, Driver},
    syscall::SystemError,
};

use super::{super::device::driver::DriverError, platform_device::PlatformDevice, CompatibleTable};

lazy_static! {
    static ref PLATFORM_COMPAT_TABLE: CompatibleTable = CompatibleTable::new(vec!["platform"]);
}
/// @brief: 实现该trait的设备驱动实例应挂载在platform总线上，
///         同时应该实现Driver trait
pub trait PlatformDriver: Driver {
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
    /// 参考 https://opengrok.ringotek.cn/xref/linux-6.1.9/drivers/base/platform.c?fi=__platform_driver_register#861
    pub fn register(&self, driver: Arc<dyn PlatformDriver>) -> Result<(), SystemError> {
        todo!()
    }
}
