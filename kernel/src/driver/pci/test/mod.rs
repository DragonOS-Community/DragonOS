use alloc::sync::Arc;
use system_error::SystemError;

use self::{pt_device::TestDevice, pt_driver::TestDriver};

use super::{
    dev_id::PciDeviceID,
    device::pci_device_manager,
    driver::{pci_driver_manager, PciDriver},
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

    let _ = pci_device_manager().device_add(tdev.clone());
    let _ = pci_driver_manager().register(tdrv.clone());
    unsafe {
        TEST_DEVICE = Some(tdev);
        TEST_DRIVER = Some(tdrv);
    }
    Ok(())
}
