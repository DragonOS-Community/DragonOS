use alloc::{
    string::ToString,
    sync::{Arc, Weak},
};
use system_error::SystemError;
use unified_init::macros::unified_init;

use crate::{
    driver::{
        base::{
            class::Class,
            device::{bus::Bus, device_manager, driver::Driver, Device, DeviceType, IdTable},
            kobject::{KObjType, KObject, LockedKObjectState},
            kset::KSet,
        },
        input::serio::serio_device::{serio_device_manager, SerioDevice},
    },
    filesystem::kernfs::KernFSInode,
    init::initcall::INITCALL_DEVICE,
    libs::spinlock::SpinLock,
};

#[derive(Debug)]
#[cast_to([sync] Device, SerioDevice)]
pub struct Ps2MouseDevice {
    inner: SpinLock<InnerPs2MouseDevice>,
    kobj_state: LockedKObjectState,
}

impl Ps2MouseDevice {
    pub const NAME: &'static str = "ps2-mouse-device";
    pub fn new() -> Self {
        let r = Self {
            inner: SpinLock::new(InnerPs2MouseDevice {
                bus: None,
                class: None,
                driver: None,
                kern_inode: None,
                parent: None,
                kset: None,
                kobj_type: None,
            }),
            kobj_state: LockedKObjectState::new(None),
        };
        return r;
    }
}

#[derive(Debug)]
struct InnerPs2MouseDevice {
    bus: Option<Weak<dyn Bus>>,
    class: Option<Arc<dyn Class>>,
    driver: Option<Weak<dyn Driver>>,
    kern_inode: Option<Arc<KernFSInode>>,
    parent: Option<Weak<dyn KObject>>,
    kset: Option<Arc<KSet>>,
    kobj_type: Option<&'static dyn KObjType>,
}

impl Device for Ps2MouseDevice {
    fn is_dead(&self) -> bool {
        false
    }

    fn dev_type(&self) -> crate::driver::base::device::DeviceType {
        DeviceType::Char
    }

    fn id_table(&self) -> crate::driver::base::device::IdTable {
        IdTable::new(self.name(), None)
    }

    fn set_bus(&self, bus: Option<alloc::sync::Weak<dyn crate::driver::base::device::bus::Bus>>) {
        self.inner.lock().bus = bus;
    }

    fn set_class(&self, class: Option<alloc::sync::Arc<dyn crate::driver::base::class::Class>>) {
        self.inner.lock().class = class;
    }

    fn driver(&self) -> Option<alloc::sync::Arc<dyn crate::driver::base::device::driver::Driver>> {
        self.inner.lock().driver.clone()?.upgrade()
    }

    fn set_driver(
        &self,
        driver: Option<alloc::sync::Weak<dyn crate::driver::base::device::driver::Driver>>,
    ) {
        self.inner.lock().driver = driver;
    }

    fn can_match(&self) -> bool {
        true
    }

    fn set_can_match(&self, _can_match: bool) {}

    fn state_synced(&self) -> bool {
        true
    }

    fn bus(&self) -> Option<alloc::sync::Weak<dyn crate::driver::base::device::bus::Bus>> {
        self.inner.lock().bus.clone()
    }

    fn class(&self) -> Option<Arc<dyn crate::driver::base::class::Class>> {
        self.inner.lock().class.clone()
    }
}

impl SerioDevice for Ps2MouseDevice {
    fn write(
        &self,
        _device: &alloc::sync::Arc<dyn SerioDevice>,
        _data: u8,
    ) -> Result<(), system_error::SystemError> {
        todo!()
    }

    fn open(
        &self,
        _device: &alloc::sync::Arc<dyn SerioDevice>,
    ) -> Result<(), system_error::SystemError> {
        todo!()
    }

    fn close(
        &self,
        _device: &alloc::sync::Arc<dyn SerioDevice>,
    ) -> Result<(), system_error::SystemError> {
        todo!()
    }

    fn start(
        &self,
        _device: &alloc::sync::Arc<dyn SerioDevice>,
    ) -> Result<(), system_error::SystemError> {
        todo!()
    }

    fn stop(
        &self,
        _device: &alloc::sync::Arc<dyn SerioDevice>,
    ) -> Result<(), system_error::SystemError> {
        todo!()
    }
}

impl KObject for Ps2MouseDevice {
    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn set_inode(&self, inode: Option<alloc::sync::Arc<crate::filesystem::kernfs::KernFSInode>>) {
        self.inner.lock().kern_inode = inode;
    }

    fn inode(&self) -> Option<alloc::sync::Arc<crate::filesystem::kernfs::KernFSInode>> {
        self.inner.lock().kern_inode.clone()
    }

    fn parent(&self) -> Option<alloc::sync::Weak<dyn KObject>> {
        self.inner.lock().parent.clone()
    }

    fn set_parent(&self, parent: Option<alloc::sync::Weak<dyn KObject>>) {
        self.inner.lock().parent = parent
    }

    fn kset(&self) -> Option<alloc::sync::Arc<crate::driver::base::kset::KSet>> {
        self.inner.lock().kset.clone()
    }

    fn set_kset(&self, kset: Option<alloc::sync::Arc<crate::driver::base::kset::KSet>>) {
        self.inner.lock().kset = kset;
    }

    fn kobj_type(&self) -> Option<&'static dyn crate::driver::base::kobject::KObjType> {
        self.inner.lock().kobj_type.clone()
    }

    fn set_kobj_type(&self, ktype: Option<&'static dyn crate::driver::base::kobject::KObjType>) {
        self.inner.lock().kobj_type = ktype;
    }

    fn name(&self) -> alloc::string::String {
        Self::NAME.to_string()
    }

    fn set_name(&self, _name: alloc::string::String) {}

    fn kobj_state(
        &self,
    ) -> crate::libs::rwlock::RwLockReadGuard<crate::driver::base::kobject::KObjectState> {
        self.kobj_state.read()
    }

    fn kobj_state_mut(
        &self,
    ) -> crate::libs::rwlock::RwLockWriteGuard<crate::driver::base::kobject::KObjectState> {
        self.kobj_state.write()
    }

    fn set_kobj_state(&self, state: crate::driver::base::kobject::KObjectState) {
        *self.kobj_state.write() = state;
    }
}

#[unified_init(INITCALL_DEVICE)]
fn ps2_mouse_device_int() -> Result<(), SystemError> {
    let device = Arc::new(Ps2MouseDevice::new());
    device_manager().device_default_initialize(&(device.clone() as Arc<dyn Device>));
    serio_device_manager().register_port(device)?;
    return Ok(());
}
