use alloc::{
    string::{String, ToString},
    sync::{Arc, Weak},
    vec::Vec,
};

use crate::{
    driver::{
        base::{
            device::{bus::Bus, driver::Driver, Device, IdTable},
            kobject::{KObjType, KObject, KObjectState, LockedKObjectState},
            kset::KSet,
        },
        pci::pci_driver::{
            dev_id::PciDeviceID,
            device::PciDevice,
            driver::{InnerPciDriver, PciDriver},
        },
    },
    filesystem::kernfs::KernFSInode,
    libs::{
        rwlock::{RwLockReadGuard, RwLockWriteGuard},
        spinlock::SpinLock,
    },
};
#[derive(Debug)]
#[cast_to([sync] PciDriver)]
pub struct TestDriver {
    inner: SpinLock<InnerPciDriver>,
    kobj_state: LockedKObjectState,
}

impl TestDriver {
    pub fn new() -> Self {
        Self {
            inner: SpinLock::new(InnerPciDriver {
                ktype: None,
                kset: None,
                parent: None,
                kernfs_inode: None,
                devices: Vec::new(),
                bus: None,
                locked_dynid_list: Vec::new(),
            }),

            kobj_state: LockedKObjectState::new(None),
        }
    }
}

impl PciDriver for TestDriver {
    fn add_dynid(&mut self, id: PciDeviceID) -> Result<(), system_error::SystemError> {
        self.inner.lock().insert_id(id);
        Ok(())
    }

    fn locked_dynid_list(&self) -> Option<Vec<Arc<PciDeviceID>>> {
        Some(self.inner.lock().id_list().clone())
    }

    fn probe(
        &self,
        _device: &Arc<dyn PciDevice>,
        _id: &PciDeviceID,
    ) -> Result<(), system_error::SystemError> {
        Ok(())
    }

    fn remove(&self, _device: &Arc<dyn PciDevice>) -> Result<(), system_error::SystemError> {
        Ok(())
    }

    fn resume(&self, _device: &Arc<dyn PciDevice>) -> Result<(), system_error::SystemError> {
        Ok(())
    }

    fn shutdown(&self, _device: &Arc<dyn PciDevice>) -> Result<(), system_error::SystemError> {
        Ok(())
    }

    fn suspend(&self, _device: &Arc<dyn PciDevice>) -> Result<(), system_error::SystemError> {
        Ok(())
    }
}

impl Driver for TestDriver {
    fn id_table(&self) -> Option<IdTable> {
        Some(IdTable::new("PciTestDriver".to_string(), None))
    }

    fn devices(&self) -> Vec<Arc<dyn Device>> {
        self.inner.lock().devices.clone()
    }

    fn add_device(&self, device: Arc<dyn Device>) {
        let mut guard = self.inner.lock();
        // check if the device is already in the list
        if guard.devices.iter().any(|dev| Arc::ptr_eq(dev, &device)) {
            return;
        }

        guard.devices.push(device);
    }

    fn delete_device(&self, device: &Arc<dyn Device>) {
        let mut guard = self.inner.lock();
        guard.devices.retain(|dev| !Arc::ptr_eq(dev, device));
    }

    fn set_bus(&self, bus: Option<Weak<dyn Bus>>) {
        self.inner.lock().bus = bus;
    }

    fn bus(&self) -> Option<Weak<dyn Bus>> {
        self.inner.lock().bus.clone()
    }
}

impl KObject for TestDriver {
    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn set_inode(&self, inode: Option<Arc<KernFSInode>>) {
        self.inner.lock().kernfs_inode = inode;
    }

    fn inode(&self) -> Option<Arc<KernFSInode>> {
        self.inner.lock().kernfs_inode.clone()
    }

    fn parent(&self) -> Option<Weak<dyn KObject>> {
        self.inner.lock().parent.clone()
    }

    fn set_parent(&self, parent: Option<Weak<dyn KObject>>) {
        self.inner.lock().parent = parent;
    }

    fn kset(&self) -> Option<Arc<KSet>> {
        self.inner.lock().kset.clone()
    }

    fn set_kset(&self, kset: Option<Arc<KSet>>) {
        self.inner.lock().kset = kset;
    }

    fn kobj_type(&self) -> Option<&'static dyn KObjType> {
        self.inner.lock().ktype
    }

    fn set_kobj_type(&self, ktype: Option<&'static dyn KObjType>) {
        self.inner.lock().ktype = ktype;
    }

    fn name(&self) -> String {
        "PciTestDriver".to_string()
    }

    fn set_name(&self, _name: String) {
        // do nothing
    }

    fn kobj_state(&self) -> RwLockReadGuard<KObjectState> {
        self.kobj_state.read()
    }

    fn kobj_state_mut(&self) -> RwLockWriteGuard<KObjectState> {
        self.kobj_state.write()
    }

    fn set_kobj_state(&self, state: KObjectState) {
        *self.kobj_state.write() = state;
    }
}
