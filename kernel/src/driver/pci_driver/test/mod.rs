use alloc::sync::Arc;
use system_error::SystemError;

use crate::driver::base::device::{device_manager, Device};

use self::{pt_device::TestDevice, pt_driver::TestDriver};

use super::{
    dev_id::PciDeviceID,
    pci_device::{pci_device_manager, PciDeviceManager},
    pci_driver::{pci_driver_manager, PciDriver},
};

pub mod pt_device;
pub mod pt_driver;

static mut TEST_DRIVER: Option<Arc<TestDriver>> = None;
static mut TEST_DEVICE: Option<Arc<TestDevice>> = None;
pub fn pt_init() -> Result<(), SystemError> {
    let tdev = Arc::new(TestDevice::new());
    let mut drv = TestDriver::new();
    drv.add_dynid(PciDeviceID::dummpy())?;
    let tdrv = Arc::new(drv);
    device_manager().device_default_initialize(&(tdev.clone() as Arc<dyn Device>));
    let _ = pci_device_manager().device_add(tdev.clone());
    let _ = pci_driver_manager().register(tdrv.clone());
    unsafe {
        TEST_DEVICE = Some(tdev);
        TEST_DRIVER = Some(tdrv);
    }
    Ok(())
}
