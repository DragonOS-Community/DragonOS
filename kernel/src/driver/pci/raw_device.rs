use core::any::Any;

use alloc::{
    string::{String, ToString},
    sync::{Arc, Weak},
};

use crate::{
    driver::base::{
        class::Class,
        device::{bus::Bus, driver::Driver, Device, DeviceType, IdTable},
        kobject::{KObjType, KObject, KObjectState, LockedKObjectState},
        kset::KSet,
    },
    filesystem::{kernfs::KernFSInode, sysfs::AttributeGroup},
    libs::rwlock::{RwLock, RwLockReadGuard, RwLockWriteGuard},
};

use super::{
    pci::PciDeviceStructureGeneralDevice,
    pci_driver::{
        attr::BasicPciReadOnlyAttrs,
        dev_id::{PciDeviceID, PciSpecifiedData},
        device::{InnerPciDevice, PciDevice},
    },
};
#[derive(Debug)]
#[cast_to([sync] Device)]
#[cast_to([sync] PciDevice)]
pub struct PciGeneralDevice {
    inner: RwLock<InnerPciDevice>,
    kobj_state: LockedKObjectState,
    dev_id: PciDeviceID,
    header: Arc<PciDeviceStructureGeneralDevice>,
}

impl From<&PciDeviceStructureGeneralDevice> for PciGeneralDevice {
    fn from(value: &PciDeviceStructureGeneralDevice) -> Self {
        let value = Arc::new(value.clone());
        let name: String = value.common_header.bus_device_function.into();
        let kobj_state = LockedKObjectState::new(None);
        let inner = RwLock::new(InnerPciDevice::default());
        let mut dev_id = PciDeviceID::dummpy();
        dev_id.set_special(PciSpecifiedData::Virtio());
        let res = Self {
            inner,
            kobj_state,
            dev_id,
            header: value,
        };
        res.set_name(name);
        res
    }
}

impl PciDevice for PciGeneralDevice {
    fn dynid(&self) -> PciDeviceID {
        self.dev_id
    }

    fn vendor(&self) -> u16 {
        self.header.common_header.vendor_id
    }

    fn device_id(&self) -> u16 {
        self.header.common_header.device_id
    }

    fn subsystem_vendor(&self) -> u16 {
        self.header.subsystem_vendor_id
    }

    fn subsystem_device(&self) -> u16 {
        self.header.subsystem_id
    }
}

impl Device for PciGeneralDevice {
    fn attribute_groups(&self) -> Option<&'static [&'static dyn AttributeGroup]> {
        Some(&[&BasicPciReadOnlyAttrs])
    }

    fn bus(&self) -> Option<Weak<dyn Bus>> {
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

    fn dev_type(&self) -> DeviceType {
        DeviceType::Pci
    }

    fn id_table(&self) -> IdTable {
        IdTable::new("testPci".to_string(), None)
    }

    fn can_match(&self) -> bool {
        true
    }

    fn is_dead(&self) -> bool {
        false
    }

    fn set_bus(&self, bus: Option<Weak<dyn Bus>>) {
        self.inner.write().set_bus(bus);
    }

    fn set_can_match(&self, _can_match: bool) {}

    fn set_class(&self, class: Option<Weak<dyn Class>>) {
        self.inner.write().set_class(class)
    }

    fn set_driver(&self, driver: Option<Weak<dyn Driver>>) {
        self.inner.write().set_driver(driver)
    }

    fn state_synced(&self) -> bool {
        true
    }
}

impl KObject for PciGeneralDevice {
    fn as_any_ref(&self) -> &dyn Any {
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
        self.inner.write().name = Some(name)
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
