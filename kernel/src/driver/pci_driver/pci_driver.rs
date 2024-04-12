use alloc::{sync::Arc, vec::Vec};
use system_error::SystemError;

use crate::{
    driver::base::device::{
        bus::Bus,
        driver::{driver_manager, Driver},
    },
    libs::rwlock::RwLockReadGuard,
};

use super::{dev_id::PciDeviceID, pci_bus, pci_device::PciDevice};

pub trait PciDriver: Driver {
    //https://code.dragonos.org.cn/xref/linux-6.1.9/drivers/net/wireless/realtek/rtw88/pci.c?fi=rtw_pci_probe#1731是一个实例
    fn probe(&self, device: &Arc<dyn PciDevice>, id: &PciDeviceID) -> Result<(), SystemError>;
    fn remove(&self, device: &Arc<dyn PciDevice>) -> Result<(), SystemError>;
    fn shutdown(&self, device: &Arc<dyn PciDevice>) -> Result<(), SystemError>;
    fn suspend(&self, device: &Arc<dyn PciDevice>) -> Result<(), SystemError>;
    fn resume(&self, device: &Arc<dyn PciDevice>) -> Result<(), SystemError>;
    fn add_dynid(&mut self, id: PciDeviceID) -> Result<(), SystemError>;
    fn locked_dynid_list(&self) -> Option<Vec<Arc<PciDeviceID>>>;
    fn match_dev(&self, dev: &Arc<dyn PciDevice>) -> Option<Arc<PciDeviceID>> {
        for i in self.locked_dynid_list()?.iter() {
            if i.match_dev(&dev) {
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
    //注册只是在驱动的结构体上表明这个驱动是哪个bus的
    pub fn register(&self, driver: Arc<dyn PciDriver>) -> Result<(), SystemError> {
        driver.set_bus(Some(Arc::downgrade(&(pci_bus() as Arc<dyn Bus>))));
        //注：这个driver_manager的register函数其实做了很多事情
        //它要求driver的bus成员必须是填入的，并且使用该bus成员进行register
        //bus要做的是实现一个match_device函数，它是一个device哈driver的双向函数，用于match
        //bus也要实现一个probe操作，它是
        return driver_manager().register(driver as Arc<dyn Driver>);
    }

    #[allow(dead_code)]
    pub fn unregister(&self, driver: &Arc<dyn PciDriver>) {
        driver_manager().unregister(&(driver.clone() as Arc<dyn Driver>));
    }
}
