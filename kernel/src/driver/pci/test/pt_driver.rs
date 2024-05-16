use alloc::{
    string::{String, ToString},
    sync::{Arc, Weak},
    vec::Vec,
};

use crate::{
    driver::{
        base::{
            device::{
                bus::Bus,
                driver::{Driver, DriverCommonData},
                Device, IdTable,
            },
            kobject::{KObjType, KObject, KObjectCommonData, KObjectState, LockedKObjectState},
            kset::KSet,
        },
        pci::{dev_id::PciDeviceID, device::PciDevice, driver::PciDriver},
    },
    filesystem::kernfs::KernFSInode,
    libs::rwlock::{RwLock, RwLockReadGuard, RwLockWriteGuard},
};
#[derive(Debug)]
#[cast_to([sync] PciDriver)]
pub struct TestDriver {
    driver_data: RwLock<DriverCommonData>,
    kobj_data: RwLock<KObjectCommonData>,
    kobj_state: LockedKObjectState,
    pub locked_dynid_list: RwLock<Vec<Arc<PciDeviceID>>>,
}

/// # 结构功能
/// 本结构体是测试用的驱动，目前暂时保留，否则将出现大量dead code
/// 在编写了实际的pci驱动后，可将该驱动删除
impl TestDriver {
    pub fn new() -> Self {
        Self {
            driver_data: RwLock::new(DriverCommonData::default()),
            kobj_data: RwLock::new(KObjectCommonData::default()),
            kobj_state: LockedKObjectState::new(None),
            locked_dynid_list: RwLock::new(vec![]),
        }
    }
}

impl PciDriver for TestDriver {
    fn add_dynid(&mut self, id: PciDeviceID) -> Result<(), system_error::SystemError> {
        let id = Arc::new(id);
        self.locked_dynid_list.write().push(id);
        Ok(())
    }

    fn locked_dynid_list(&self) -> Option<Vec<Arc<PciDeviceID>>> {
        Some(self.locked_dynid_list.read().clone())
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
        self.driver_data.read().devices.clone()
    }

    fn add_device(&self, device: Arc<dyn Device>) {
        let mut guard = self.driver_data.write();
        // check if the device is already in the list
        if guard.devices.iter().any(|dev| Arc::ptr_eq(dev, &device)) {
            return;
        }

        guard.devices.push(device);
    }

    fn delete_device(&self, device: &Arc<dyn Device>) {
        let mut guard = self.driver_data.write();
        guard.devices.retain(|dev| !Arc::ptr_eq(dev, device));
    }

    fn set_bus(&self, bus: Option<Weak<dyn Bus>>) {
        self.driver_data.write().bus = bus;
    }

    fn bus(&self) -> Option<Weak<dyn Bus>> {
        self.driver_data.read().bus.clone()
    }
}

impl KObject for TestDriver {
    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn set_inode(&self, inode: Option<Arc<KernFSInode>>) {
        self.kobj_data.write().kern_inode = inode;
    }

    fn inode(&self) -> Option<Arc<KernFSInode>> {
        self.kobj_data.read().kern_inode.clone()
    }

    fn parent(&self) -> Option<Weak<dyn KObject>> {
        self.kobj_data.read().parent.clone()
    }

    fn set_parent(&self, parent: Option<Weak<dyn KObject>>) {
        self.kobj_data.write().parent = parent;
    }

    fn kset(&self) -> Option<Arc<KSet>> {
        self.kobj_data.read().kset.clone()
    }

    fn set_kset(&self, kset: Option<Arc<KSet>>) {
        self.kobj_data.write().kset = kset;
    }

    fn kobj_type(&self) -> Option<&'static dyn KObjType> {
        self.kobj_data.read().kobj_type
    }

    fn set_kobj_type(&self, ktype: Option<&'static dyn KObjType>) {
        self.kobj_data.write().kobj_type = ktype;
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
