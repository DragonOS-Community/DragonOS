use core::sync::atomic::AtomicU16;

use alloc::{
    string::{String, ToString},
    sync::{Arc, Weak},
};
use system_error::SystemError;

use crate::{
    driver::{
        base::{
            class::Class,
            device::{bus::Bus, driver::Driver, Device, DeviceType, IdTable},
            kobject::{KObjType, KObject, KObjectState, LockedKObjectState},
            kset::KSet,
        },
        pci_driver::{dev_id::PciDeviceID, pci_device::{pci_device_manager, PciDevice}, test::pt_device::InnerPciDevice},
    },
    filesystem::kernfs::KernFSInode,
    libs::
        rwlock::{RwLock, RwLockReadGuard, RwLockWriteGuard}
    ,
};

use super::pci::{ PciDeviceStructureGeneralDevice, PciDeviceStructureHeader, PCI_DEVICE_LINKEDLIST};
static NAME_SEQ: AtomicU16 = AtomicU16::new(0);
#[derive(Debug)]
pub struct PciRawGeneralDevice {
    inner: RwLock<InnerPciDevice>,
    kobj_state: LockedKObjectState,
    dev_id: PciDeviceID,
}

impl PciRawGeneralDevice {
    
}

impl From<&PciDeviceStructureGeneralDevice> for PciRawGeneralDevice {
    fn from(value: &PciDeviceStructureGeneralDevice) -> Self {
        let value=&value.common_header;
        let kobj_state = LockedKObjectState::new(None);
        let inner = RwLock::new(InnerPciDevice::default());
        let dev_id=PciDeviceID::new(value.vendor_id,value.device_id,0,0,value.class_code,0,0,0);
        let seq=NAME_SEQ.load(core::sync::atomic::Ordering::SeqCst);
        let name=format!("PciRaw{:?}",seq);
        NAME_SEQ.store(seq+1, core::sync::atomic::Ordering::SeqCst);
        let res=Self { inner, kobj_state, dev_id};
        res.set_name(name);
        res
    }
}

impl PciDevice for PciRawGeneralDevice {
    fn dynid(&self) -> crate::driver::pci_driver::dev_id::PciDeviceID {
        todo!()
    }
}

impl Device for PciRawGeneralDevice {
    fn attribute_groups(
        &self,
    ) -> Option<&'static [&'static dyn crate::filesystem::sysfs::AttributeGroup]> {
        None
    }

    fn bus(&self) -> Option<alloc::sync::Weak<dyn crate::driver::base::device::bus::Bus>> {
        self.inner.read().bus()
    }

    fn class(&self) -> Option<Arc<dyn Class>> {
        let mut guard = self.inner.write();
        let r = guard.class.clone()?.upgrade();
        if r.is_none() {
            guard.class = None;
        }

        return r;
    }

    fn driver(&self) -> Option<Arc<dyn Driver>> {
        self.inner.read().driver.clone()?.upgrade()
    }

    fn dev_type(&self) -> crate::driver::base::device::DeviceType {
        DeviceType::Pci
    }

    fn id_table(&self) -> crate::driver::base::device::IdTable {
        IdTable::new("testPci".to_string(), None)
    }

    fn can_match(&self) -> bool {
        true
    }

    fn is_dead(&self) -> bool {
        false
    }

    fn set_bus(&self, bus: Option<alloc::sync::Weak<dyn Bus>>) {
        self.inner.write().set_bus(bus);
    }

    fn set_can_match(&self, can_match: bool) {
        
    }

    fn set_class(&self, class: Option<alloc::sync::Weak<dyn Class>>) {
        self.inner.write().set_class(class)
    }

    fn set_driver(&self, driver: Option<alloc::sync::Weak<dyn Driver>>) {
        self.inner.write().set_driver(driver)
    }

    fn state_synced(&self) -> bool {
        true
    }
}

impl KObject for PciRawGeneralDevice {
    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn set_inode(&self, inode: Option<Arc<KernFSInode>>) {
        self.inner.write().kern_inode = inode;
    }

    fn inode(&self) -> Option<Arc<KernFSInode>> {
        self.inner.read().kern_inode.clone()
    }

    fn parent(&self) -> Option<Weak<dyn KObject>> {
        self.inner.read().parent.clone()
    }

    fn set_parent(&self, parent: Option<Weak<dyn KObject>>) {
        self.inner.write().parent = parent;
    }

    fn kset(&self) -> Option<Arc<KSet>> {
        self.inner.read().kset.clone()
    }

    fn set_kset(&self, kset: Option<Arc<KSet>>) {
        self.inner.write().kset = kset;
    }

    fn kobj_type(&self) -> Option<&'static dyn KObjType> {
        self.inner.read().kobj_type
    }

    fn set_kobj_type(&self, ktype: Option<&'static dyn KObjType>) {
        self.inner.write().kobj_type = ktype;
    }

    fn name(&self) -> String {
        self.inner.read().name.clone().unwrap()
    }

    fn set_name(&self, name: String) {
        self.inner.write().name=Some(name)
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

#[inline(never)]
pub fn pci_device_search(){
    for i in PCI_DEVICE_LINKEDLIST.read().iter(){
        if let Some(dev) = i.as_standard_device(){
            let raw_dev=PciRawGeneralDevice::from(dev);
            let _ =pci_device_manager().device_add(Arc::new(raw_dev));
        }else{
            continue;
        }
    }
}