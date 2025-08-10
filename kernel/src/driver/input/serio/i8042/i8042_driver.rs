use alloc::{
    string::{String, ToString},
    sync::{Arc, Weak},
    vec::Vec,
};
use system_error::SystemError;

use crate::{
    driver::base::{
        device::{bus::Bus, driver::Driver, Device, IdTable},
        kobject::{KObjType, KObject, KObjectState, LockedKObjectState},
        kset::KSet,
        platform::{platform_device::PlatformDevice, platform_driver::PlatformDriver},
    },
    filesystem::kernfs::KernFSInode,
    libs::{
        rwlock::{RwLockReadGuard, RwLockWriteGuard},
        spinlock::SpinLock,
    },
};

use super::{i8042_device::I8042PlatformDevice, i8042_setup_aux};

#[derive(Debug)]
#[cast_to([sync] PlatformDriver)]
pub struct I8042Driver {
    inner: SpinLock<InnerI8042Driver>,
    kobj_state: LockedKObjectState,
}

impl I8042Driver {
    pub const NAME: &'static str = "i8042";
    pub fn new() -> Arc<I8042Driver> {
        let r = Arc::new(Self {
            inner: SpinLock::new(InnerI8042Driver {
                ktype: None,
                kset: None,
                parent: None,
                kernfs_inode: None,
                devices: Vec::new(),
                bus: None,
                self_ref: Weak::new(),
            }),
            kobj_state: LockedKObjectState::new(None),
        });

        r.inner.lock().self_ref = Arc::downgrade(&r);

        return r;
    }
}

#[derive(Debug)]
pub struct InnerI8042Driver {
    ktype: Option<&'static dyn KObjType>,
    kset: Option<Arc<KSet>>,
    parent: Option<Weak<dyn KObject>>,
    kernfs_inode: Option<Arc<KernFSInode>>,
    devices: Vec<Arc<dyn Device>>,
    bus: Option<Weak<dyn Bus>>,

    self_ref: Weak<I8042Driver>,
}

impl PlatformDriver for I8042Driver {
    // TODO: https://code.dragonos.org.cn/xref/linux-6.1.9/drivers/input/serio/i8042.c#1542
    fn probe(&self, device: &Arc<dyn PlatformDevice>) -> Result<(), SystemError> {
        let device = device
            .clone()
            .arc_any()
            .downcast::<I8042PlatformDevice>()
            .map_err(|_| SystemError::EINVAL)?;

        device.set_driver(Some(self.inner.lock().self_ref.clone()));

        i8042_setup_aux()?;
        return Ok(());
    }

    // TODO: https://code.dragonos.org.cn/xref/linux-6.1.9/drivers/input/serio/i8042.c#1587
    fn remove(&self, _device: &Arc<dyn PlatformDevice>) -> Result<(), SystemError> {
        todo!()
    }

    // TODO: https://code.dragonos.org.cn/xref/linux-6.1.9/drivers/input/serio/i8042.c#1322
    fn shutdown(&self, _device: &Arc<dyn PlatformDevice>) -> Result<(), SystemError> {
        todo!()
    }

    fn suspend(&self, _device: &Arc<dyn PlatformDevice>) -> Result<(), SystemError> {
        // do nothing
        return Ok(());
    }

    fn resume(&self, _device: &Arc<dyn PlatformDevice>) -> Result<(), SystemError> {
        // do nothing
        return Ok(());
    }
}

impl Driver for I8042Driver {
    fn id_table(&self) -> Option<IdTable> {
        Some(IdTable::new(I8042PlatformDevice::NAME.to_string(), None))
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

impl KObject for I8042Driver {
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
