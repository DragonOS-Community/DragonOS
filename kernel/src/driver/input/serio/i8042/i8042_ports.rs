use alloc::{
    string::{String, ToString},
    sync::{Arc, Weak},
};
use system_error::SystemError;

use crate::{
    driver::{
        base::{
            class::Class,
            device::{bus::Bus, driver::Driver, Device, DeviceCommonData, DeviceType, IdTable},
            kobject::{KObjType, KObject, KObjectCommonData, KObjectState, LockedKObjectState},
            kset::KSet,
        },
        input::serio::serio_device::SerioDevice,
    },
    filesystem::kernfs::KernFSInode,
    libs::{
        rwlock::{RwLockReadGuard, RwLockWriteGuard},
        spinlock::{SpinLock, SpinLockGuard},
    },
};

use super::{i8042_start, i8042_stop};

#[derive(Debug)]
#[cast_to([sync] Device)]
pub struct I8042AuxPort {
    inner: SpinLock<InnerI8042AuxPort>,
    kobj_state: LockedKObjectState,
}

#[derive(Debug)]
pub struct InnerI8042AuxPort {
    device_common: DeviceCommonData,
    kobject_common: KObjectCommonData,
}

impl I8042AuxPort {
    pub const NAME: &'static str = "serio1";
    pub fn new() -> Self {
        return Self {
            inner: SpinLock::new(InnerI8042AuxPort {
                device_common: DeviceCommonData::default(),
                kobject_common: KObjectCommonData::default(),
            }),
            kobj_state: LockedKObjectState::new(None),
        };
    }

    fn inner(&self) -> SpinLockGuard<InnerI8042AuxPort> {
        self.inner.lock()
    }
}

impl Device for I8042AuxPort {
    fn dev_type(&self) -> DeviceType {
        DeviceType::Char
    }

    fn id_table(&self) -> IdTable {
        IdTable::new(self.name(), None)
    }

    fn bus(&self) -> Option<Weak<dyn Bus>> {
        self.inner().device_common.bus.clone()
    }

    fn set_bus(&self, bus: Option<Weak<dyn Bus>>) {
        self.inner().device_common.bus = bus;
    }

    fn set_class(&self, class: Option<Weak<dyn Class>>) {
        self.inner().device_common.class = class;
    }

    fn class(&self) -> Option<Arc<dyn Class>> {
        let mut guard = self.inner();
        let r = guard.device_common.class.clone()?.upgrade();
        if r.is_none() {
            guard.device_common.class = None;
        }
        return r;
    }

    fn driver(&self) -> Option<Arc<dyn Driver>> {
        self.inner().device_common.driver.clone()?.upgrade()
    }

    fn set_driver(&self, driver: Option<Weak<dyn Driver>>) {
        self.inner().device_common.driver = driver;
    }

    fn is_dead(&self) -> bool {
        false
    }

    fn can_match(&self) -> bool {
        true
    }

    fn set_can_match(&self, _can_match: bool) {}

    fn state_synced(&self) -> bool {
        true
    }

    fn dev_parent(&self) -> Option<Weak<dyn Device>> {
        self.inner().device_common.get_parent_weak_or_clear()
    }

    fn set_dev_parent(&self, parent: Option<Weak<dyn Device>>) {
        self.inner().device_common.parent = parent;
    }
}

impl KObject for I8042AuxPort {
    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn set_inode(&self, inode: Option<Arc<KernFSInode>>) {
        self.inner().kobject_common.kern_inode = inode;
    }

    fn inode(&self) -> Option<Arc<KernFSInode>> {
        self.inner().kobject_common.kern_inode.clone()
    }

    fn parent(&self) -> Option<Weak<dyn KObject>> {
        self.inner().kobject_common.parent.clone()
    }

    fn set_parent(&self, parent: Option<Weak<dyn KObject>>) {
        self.inner().kobject_common.parent = parent;
    }

    fn kset(&self) -> Option<Arc<KSet>> {
        self.inner().kobject_common.kset.clone()
    }

    fn set_kset(&self, kset: Option<Arc<KSet>>) {
        self.inner().kobject_common.kset = kset;
    }

    fn kobj_type(&self) -> Option<&'static dyn KObjType> {
        self.inner().kobject_common.kobj_type
    }

    fn set_kobj_type(&self, ktype: Option<&'static dyn KObjType>) {
        self.inner().kobject_common.kobj_type = ktype;
    }

    fn name(&self) -> String {
        Self::NAME.to_string()
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

impl SerioDevice for I8042AuxPort {
    // TODO: https://code.dragonos.org.cn/xref/linux-6.1.9/drivers/input/serio/i8042.c#387
    fn write(&self, _device: &Arc<dyn SerioDevice>, _data: u8) -> Result<(), SystemError> {
        todo!()
    }

    fn open(&self, _device: &Arc<dyn SerioDevice>) -> Result<(), SystemError> {
        Ok(())
    }

    fn close(&self, _device: &Arc<dyn SerioDevice>) -> Result<(), SystemError> {
        Ok(())
    }

    fn start(&self, device: &Arc<dyn SerioDevice>) -> Result<(), SystemError> {
        i8042_start(device)
    }

    fn stop(&self, device: &Arc<dyn SerioDevice>) -> Result<(), SystemError> {
        i8042_stop(device)
    }
}
