use alloc::sync::Arc;
use system_error::SystemError;

use crate::driver::base::device::{bus::Bus, driver::{driver_manager, Driver}};

use super::{pci_bus, pci_device::PciDevice};

pub trait PciDriver: Driver {
    fn probe(&self, device: &Arc<dyn PciDevice>) -> Result<(), SystemError>;
    fn remove(&self, device: &Arc<dyn PciDevice>) -> Result<(), SystemError>;
    fn shutdown(&self, device: &Arc<dyn PciDevice>) -> Result<(), SystemError>;
    fn suspend(&self, device: &Arc<dyn PciDevice>) -> Result<(), SystemError>;
    fn resume(&self, device: &Arc<dyn PciDevice>) -> Result<(), SystemError>;
}

pub struct PciDriverManager;

pub fn pci_driver_manager()->&'static PciDriverManager{
    &PciDriverManager
}

impl PciDriverManager {
    //注册只是在驱动的结构体上表明这个驱动是哪个bus的
    pub fn register(&self, driver: Arc<dyn PciDriver>) -> Result<(), SystemError> {
        driver.set_bus(Some(Arc::downgrade(&(pci_bus() as Arc<dyn Bus>))));
        return driver_manager().register(driver as Arc<dyn Driver>);
    }

    #[allow(dead_code)]
    pub fn unregister(&self, driver: &Arc<dyn PciDriver>) {
        driver_manager().unregister(&(driver.clone() as Arc<dyn Driver>));
    }
}
