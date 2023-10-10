use alloc::sync::Arc;

use crate::driver::base::char::CharDevOps;
use crate::driver::base::device::{device_manager, Device, DeviceResource};
use crate::driver::base::kobject::KObject;
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
impl KObject for UartDriver {
    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn set_inode(&self, inode: Option<Arc<crate::filesystem::kernfs::KernFSInode>>) {
        todo!()
    }

    fn inode(&self) -> Option<Arc<crate::filesystem::kernfs::KernFSInode>> {
        todo!()
    }

    fn parent(&self) -> Option<alloc::sync::Weak<dyn KObject>> {
        todo!()
    }

    fn set_parent(&self, parent: Option<alloc::sync::Weak<dyn KObject>>) {
        todo!()
    }

    fn kset(&self) -> Option<Arc<crate::driver::base::kset::KSet>> {
        todo!()
    }

    fn set_kset(&self, kset: Option<Arc<crate::driver::base::kset::KSet>>) {
        todo!()
    }

    fn kobj_type(&self) -> Option<&'static dyn crate::driver::base::kobject::KObjType> {
        todo!()
    }

    fn name(&self) -> alloc::string::String {
        todo!()
    }

    fn set_name(&self, name: alloc::string::String) {
        todo!()
    }

    fn kobj_state(
        &self,
    ) -> crate::libs::rwlock::RwLockReadGuard<crate::driver::base::kobject::KObjectState> {
        todo!()
    }

    fn kobj_state_mut(
        &self,
    ) -> crate::libs::rwlock::RwLockWriteGuard<crate::driver::base::kobject::KObjectState> {
        todo!()
    }

    fn set_kobj_state(&self, state: crate::driver::base::kobject::KObjectState) {
        todo!()
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
        if let Some(device) = device_manager().find_device_by_idtable(data.id_table()) {
            return Ok(device.clone());
        }
        let compatible_table = data.compatible_table();
        if compatible_table.matches(&UART_COMPAT_TABLE) {
            let device = LockedUart::default();
            let arc_device = Arc::new(device);
            device_manager()
                .add_device(arc_device.clone())
                .map_err(|_| DriverError::RegisterError)?;
            CharDevOps::cdev_add(arc_device.clone(), data.id_table().clone(), 1)
                .map_err(|_| DriverError::RegisterError)?;
        }

        return Err(DriverError::RegisterError);
    }

    fn id_table(&self) -> IdTable {
        let driver = self.0.lock();
        return driver.id_table.clone();
    }
}
