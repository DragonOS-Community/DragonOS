use alloc::{sync::Arc, vec::Vec};
use system_error::SystemError;

use crate::driver::base::device::{
    bus::Bus,
    driver::{driver_manager, Driver},
};

use super::{dev_id::PciDeviceID, device::PciDevice, subsys::pci_bus};

/// # trait功能
/// Pci驱动应该实现的trait
///
/// 参考 https://code.dragonos.org.cn/xref/linux-6.1.9/include/linux/pci.h#907
#[allow(dead_code)]
pub trait PciDriver: Driver {
    /// # 函数的功能
    /// 对设备进行probe操作
    ///
    /// ## 参数:
    /// - 'device' :要进行probe的设备
    /// - 'id' :设备的ID（暂时不清楚为什么需要这个，依Linux实现是有ID的)
    ///
    /// ## 返回值:
    /// - Ok:probe成功
    /// - Err:probe失败
    fn probe(&self, device: &Arc<dyn PciDevice>, id: &PciDeviceID) -> Result<(), SystemError>;
    fn remove(&self, device: &Arc<dyn PciDevice>) -> Result<(), SystemError>;
    fn shutdown(&self, device: &Arc<dyn PciDevice>) -> Result<(), SystemError>;
    fn suspend(&self, device: &Arc<dyn PciDevice>) -> Result<(), SystemError>;
    fn resume(&self, device: &Arc<dyn PciDevice>) -> Result<(), SystemError>;
    /// # 函数的功能
    /// 向驱动中加入一个PciDeviceID，表示该驱动可以支持该ID的设备
    ///
    /// ## 参数:
    /// - 'id' :要添加的ID
    ///
    /// ## 返回值:
    /// - 'Ok':添加成功
    /// - 'Err':添加失败
    fn add_dynid(&mut self, id: PciDeviceID) -> Result<(), SystemError>;
    /// # 函数的功能
    /// 每个Pci驱动都应该持有一个支持ID的列表，并通过该函数进行访问
    ///
    /// ## 返回值:
    /// - 'Some(Vec)': 支持ID的列表
    /// - 'None':未能获取列表
    fn locked_dynid_list(&self) -> Option<Vec<Arc<PciDeviceID>>>;
    /// # 函数的功能
    /// 检测当前驱动是否支持目标设备
    ///
    /// ## 参数:
    /// - 'dev' :要检测的设备
    ///
    /// ## 返回值:
    /// - 'Some(Arc<PciDeviceID>)': 如果支持，则返回支持的ID
    /// - 'None': 不支持的设备
    fn match_dev(&self, dev: &Arc<dyn PciDevice>) -> Option<Arc<PciDeviceID>> {
        for i in self.locked_dynid_list()?.iter() {
            if i.match_dev(dev) {
                return Some(i.clone());
            }
        }
        return None;
    }
}

pub struct PciDriverManager;

pub fn pci_driver_manager() -> &'static PciDriverManager {
    &PciDriverManager
}

impl PciDriverManager {
    pub fn register(&self, driver: Arc<dyn PciDriver>) -> Result<(), SystemError> {
        driver.set_bus(Some(Arc::downgrade(&(pci_bus() as Arc<dyn Bus>))));
        return driver_manager().register(driver as Arc<dyn Driver>);
    }

    #[allow(dead_code)]
    pub fn unregister(&self, driver: &Arc<dyn PciDriver>) {
        driver_manager().unregister(&(driver.clone() as Arc<dyn Driver>));
    }
}
