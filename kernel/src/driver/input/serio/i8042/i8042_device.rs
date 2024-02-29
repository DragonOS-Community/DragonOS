use alloc::{
    string::{String, ToString},
    sync::{Arc, Weak},
};

use crate::{
    driver::base::{
        class::Class,
        device::{bus::Bus, driver::Driver, Device, DeviceState, DeviceType, IdTable},
        kobject::{KObjType, KObject, KObjectState, LockedKObjectState},
        kset::KSet,
        platform::{platform_device::PlatformDevice, CompatibleTable},
    },
    filesystem::kernfs::KernFSInode,
    libs::{
        rwlock::{RwLockReadGuard, RwLockWriteGuard},
        spinlock::SpinLock,
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
                bus: None,
                class: None,
                driver: None,
                kern_inode: None,
                parent: None,
                kset: None,
                kobj_type: None,
                device_state: DeviceState::NotInitialized,
                pdev_id: 0,
                pdev_id_auto: false,
            }),
            kobj_state: LockedKObjectState::new(None),
        };
    }
}

#[derive(Debug)]
pub struct InnerI8042PlatformDevice {
    bus: Option<Weak<dyn Bus>>,
    class: Option<Arc<dyn Class>>,
    driver: Option<Weak<dyn Driver>>,
    kern_inode: Option<Arc<KernFSInode>>,
    parent: Option<Weak<dyn KObject>>,
    kset: Option<Arc<KSet>>,
    kobj_type: Option<&'static dyn KObjType>,
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
        self.inner.lock().bus.clone()
    }

    fn set_bus(&self, bus: Option<Weak<dyn Bus>>) {
        self.inner.lock().bus = bus;
    }

    fn set_class(&self, class: Option<Arc<dyn Class>>) {
        self.inner.lock().class = class;
    }

    fn driver(&self) -> Option<Arc<dyn Driver>> {
        self.inner.lock().driver.clone()?.upgrade()
    }

    fn set_driver(&self, driver: Option<Weak<dyn Driver>>) {
        self.inner.lock().driver = driver;
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
}

impl KObject for I8042PlatformDevice {
    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn set_inode(&self, inode: Option<Arc<KernFSInode>>) {
        self.inner.lock().kern_inode = inode;
    }

    fn inode(&self) -> Option<Arc<KernFSInode>> {
        self.inner.lock().kern_inode.clone()
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
        self.inner.lock().kobj_type
    }

    fn set_kobj_type(&self, ktype: Option<&'static dyn KObjType>) {
        self.inner.lock().kobj_type = ktype;
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

    fn compatible_table(&self) -> CompatibleTable {
        todo!()
    }

    fn is_initialized(&self) -> bool {
        self.inner.lock().device_state == DeviceState::Initialized
    }

    fn set_state(&self, set_state: DeviceState) {
        self.inner.lock().device_state = set_state;
    }
}
