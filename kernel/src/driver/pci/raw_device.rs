use core::any::Any;

use alloc::{
    string::{String, ToString},
    sync::{Arc, Weak},
};

use crate::{
    driver::base::{
        class::Class,
        device::{bus::Bus, driver::Driver, Device, DeviceCommonData, DeviceType, IdTable},
        kobject::{KObjType, KObject, KObjectCommonData, KObjectState, LockedKObjectState},
        kset::KSet,
    },
    filesystem::{kernfs::KernFSInode, sysfs::AttributeGroup},
    libs::rwlock::{RwLock, RwLockReadGuard, RwLockWriteGuard},
};

use super::{
    attr::BasicPciReadOnlyAttrs, dev_id::PciDeviceID, device::PciDevice,
    pci::PciDeviceStructureGeneralDevice,
};
#[derive(Debug)]
#[cast_to([sync] Device)]
#[cast_to([sync] PciDevice)]
pub struct PciGeneralDevice {
    device_data: RwLock<DeviceCommonData>,
    kobj_data: RwLock<KObjectCommonData>,
    name: RwLock<Option<String>>,
    kobj_state: LockedKObjectState,
    dev_id: PciDeviceID,
    header: Arc<PciDeviceStructureGeneralDevice>,
}

impl From<&PciDeviceStructureGeneralDevice> for PciGeneralDevice {
    fn from(value: &PciDeviceStructureGeneralDevice) -> Self {
        let value = Arc::new(value.clone());
        let name: String = value.common_header.bus_device_function.into();
        let kobj_state = LockedKObjectState::new(None);
        let common_dev = RwLock::new(DeviceCommonData::default());
        let common_kobj = RwLock::new(KObjectCommonData::default());
        let dev_id = PciDeviceID::dummpy();

        // dev_id.set_special(PciSpecifiedData::Virtio());
        let res = Self {
            device_data: common_dev,
            kobj_data: common_kobj,
            kobj_state,
            dev_id,
            header: value,
            name: RwLock::new(None),
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
        self.device_data.read().bus.clone()
    }

    fn class(&self) -> Option<Arc<dyn Class>> {
        let mut guard = self.device_data.write();
        let r = guard.class.clone()?.upgrade();
        if r.is_none() {
            guard.class = None;
        }

        return r;
    }

    fn driver(&self) -> Option<Arc<dyn Driver>> {
        self.device_data.read().driver.clone()?.upgrade()
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
        self.device_data.write().bus = bus;
    }

    fn set_can_match(&self, _can_match: bool) {}

    fn set_class(&self, class: Option<Weak<dyn Class>>) {
        self.device_data.write().class = class;
    }

    fn set_driver(&self, driver: Option<Weak<dyn Driver>>) {
        self.device_data.write().driver = driver
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
        self.name.read().clone().unwrap()
    }

    fn set_name(&self, name: String) {
        *self.name.write() = Some(name);
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
