use alloc::{
    string::{String, ToString},
    sync::{Arc, Weak},
};

use crate::{
    driver::base::{
        class::Class,
        device::{
            bus::Bus, driver::Driver, Device, DeviceCommonData, DeviceState, DeviceType, IdTable,
        },
        kobject::{KObjType, KObject, KObjectCommonData, KObjectState, LockedKObjectState},
        kset::KSet,
        platform::platform_device::PlatformDevice,
    },
    filesystem::kernfs::KernFSInode,
    libs::{
        rwlock::{RwLockReadGuard, RwLockWriteGuard},
        spinlock::{SpinLock, SpinLockGuard},
    },
};

#[derive(Debug)]
#[cast_to([sync] Device)]
#[cast_to([sync] PlatformDevice)]
pub struct I8042PlatformDevice {
    inner: SpinLock<InnerI8042PlatformDevice>,
    kobj_state: LockedKObjectState,
}

impl I8042PlatformDevice {
    pub const NAME: &'static str = "i8042";
    pub fn new() -> Self {
        return Self {
            inner: SpinLock::new(InnerI8042PlatformDevice {
                kobject_common: KObjectCommonData::default(),
                device_common: DeviceCommonData::default(),
                device_state: DeviceState::NotInitialized,
                pdev_id: 0,
                pdev_id_auto: false,
            }),
            kobj_state: LockedKObjectState::new(None),
        };
    }

    fn inner(&self) -> SpinLockGuard<InnerI8042PlatformDevice> {
        self.inner.lock()
    }
}

#[derive(Debug)]
pub struct InnerI8042PlatformDevice {
    kobject_common: KObjectCommonData,
    device_common: DeviceCommonData,
    device_state: DeviceState,
    pdev_id: i32,
    pdev_id_auto: bool,
}

impl Device for I8042PlatformDevice {
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
    fn class(&self) -> Option<Arc<dyn Class>> {
        let mut guard = self.inner();
        let r = guard.device_common.class.clone()?.upgrade();
        if r.is_none() {
            guard.device_common.class = None;
        }

        return r;
    }
    fn set_class(&self, class: Option<Weak<dyn Class>>) {
        self.inner().device_common.class = class;
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

    fn set_dev_parent(&self, dev_parent: Option<Weak<dyn Device>>) {
        self.inner().device_common.parent = dev_parent;
    }
}

impl KObject for I8042PlatformDevice {
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

impl PlatformDevice for I8042PlatformDevice {
    fn pdev_name(&self) -> &str {
        Self::NAME
    }

    fn set_pdev_id(&self, id: i32) {
        self.inner.lock().pdev_id = id;
    }

    fn set_pdev_id_auto(&self, id_auto: bool) {
        self.inner.lock().pdev_id_auto = id_auto;
    }

    fn is_initialized(&self) -> bool {
        self.inner.lock().device_state == DeviceState::Initialized
    }

    fn set_state(&self, set_state: DeviceState) {
        self.inner.lock().device_state = set_state;
    }
}
