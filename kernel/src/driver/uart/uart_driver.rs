use alloc::sync::Arc;

use crate::driver::base::char::CharDevOps;
use crate::driver::base::device::{Device, DeviceResource, DEVICE_MANAGER};
use crate::driver::base::platform::CompatibleTable;
use crate::{
    driver::{
        base::device::{driver::DriverError, DevicePrivateData, IdTable},
        Driver,
    },
    libs::spinlock::SpinLock,
};

use super::uart_device::LockedUart;

lazy_static! {
    pub static ref UART_COMPAT_TABLE: CompatibleTable = CompatibleTable::new(vec!["uart"]);
}

#[derive(Debug)]
pub struct InnerUartDriver {
    id_table: IdTable,
}

#[derive(Debug)]
pub struct UartDriver(SpinLock<InnerUartDriver>);

impl Default for UartDriver {
    fn default() -> Self {
        Self(SpinLock::new(InnerUartDriver {
            id_table: IdTable::default(),
        }))
    }
}

impl Driver for UartDriver {
    fn probe(&self, data: &DevicePrivateData) -> Result<(), DriverError> {
        let compatible_table = data.compatible_table();
        if compatible_table.matches(&UART_COMPAT_TABLE) {
            return Ok(());
        }

        return Err(DriverError::ProbeError);
    }

    fn load(
        &self,
        data: DevicePrivateData,
        _resource: Option<DeviceResource>,
    ) -> Result<Arc<dyn Device>, DriverError> {
        if let Some(device) = DEVICE_MANAGER.get_device(data.id_table()) {
            return Ok(device.clone());
        }
        let compatible_table = data.compatible_table();
        if compatible_table.matches(&UART_COMPAT_TABLE) {
            let device = LockedUart::default();
            let arc_device = Arc::new(device);
            DEVICE_MANAGER.add_device(data.id_table().clone(), arc_device.clone());
            CharDevOps::cdev_add(arc_device.clone(), data.id_table().clone(), 1);
        }

        return Err(DriverError::ProbeError);
    }

    fn id_table(&self) -> IdTable {
        let driver = self.0.lock();
        return driver.id_table.clone();
    }
}
