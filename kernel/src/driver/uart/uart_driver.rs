use alloc::sync::Arc;

use crate::driver::base::device::{Device, DeviceResource, DEVICE_MANAGER};
use crate::driver::base::map::CharDevOps;
use crate::driver::base::platform::CompatibleTable;
use crate::{
    driver::{
        base::device::{driver::DriverError, DevicePrivateData, IdTable},
        Driver,
    },
    libs::spinlock::SpinLock,
};

use super::uart_device::LockedUart;

#[derive(Debug)]
pub struct InnerUartDriver {
    id_table: Option<IdTable>,
}

#[derive(Debug)]
pub struct UartDriver(SpinLock<InnerUartDriver>);

impl Default for UartDriver {
    fn default() -> Self {
        Self(SpinLock::new(InnerUartDriver { id_table: None }))
    }
}

impl Driver for UartDriver {
    fn probe(&self, data: DevicePrivateData) -> Result<(), DriverError> {
        if let Some(compatible_table) = data.compatible_table() {
            if compatible_table.matches(&CompatibleTable::new(vec!["uart"])) {
                return Ok(());
            }
        }
        return Err(DriverError::ProbeError);
    }

    fn load(
        &self,
        data: DevicePrivateData,
        resource: Option<DeviceResource>,
    ) -> Result<Arc<dyn Device>, DriverError> {
        if let Some(compatible_table) = data.compatible_table() {
            if compatible_table.matches(&CompatibleTable::new(vec!["uart"])) {
                let device = LockedUart::default();
                DEVICE_MANAGER.add_device(data.id_table().clone(), Arc::new(device));
                CharDevOps::cdev_add(Arc::new(device), data.id_table().clone(), 1);
            }
        }
        return Err(DriverError::ProbeError);
    }

    fn id_table(&self) -> Result<IdTable, DriverError> {
        let driver = self.0.lock();
        if self.0.lock().id_table.is_none() {
            return Err(DriverError::UnInitialized);
        } else {
            return Ok(driver.id_table.unwrap().clone());
        }
    }

    fn as_any_ref(&self) -> &dyn core::any::Any {
        return self;
    }
}
