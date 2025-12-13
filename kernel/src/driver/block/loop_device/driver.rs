use crate::{
    driver::base::{
        device::{
            bus::Bus,
            driver::{Driver, DriverCommonData},
            Device, IdTable,
        },
        kobject::{KObjType, KObject, KObjectCommonData, KObjectState, LockedKObjectState},
        kset::KSet,
    },
    filesystem::kernfs::KernFSInode,
    libs::{
        rwlock::{RwLockReadGuard, RwLockWriteGuard},
        spinlock::{SpinLock, SpinLockGuard},
    },
};
use alloc::{
    string::{String, ToString},
    sync::{Arc, Weak},
    vec::Vec,
};
use core::{
    any::Any,
    fmt::{Debug, Formatter},
};

use super::constants::LOOP_BASENAME;

/// Loop设备驱动
/// 参考Virtio_blk驱动的实现
#[derive(Debug)]
#[cast_to([sync] Driver)]
pub struct LoopDeviceDriver {
    inner: SpinLock<InnerLoopDeviceDriver>,
    kobj_state: LockedKObjectState,
}

struct InnerLoopDeviceDriver {
    driver_common: DriverCommonData,
    kobj_common: KObjectCommonData,
}

impl Debug for InnerLoopDeviceDriver {
    fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("InnerLoopDeviceDriver")
            .field("driver_common", &self.driver_common)
            .field("kobj_common", &self.kobj_common)
            .finish()
    }
}

impl LoopDeviceDriver {
    pub fn new() -> Arc<Self> {
        let inner = InnerLoopDeviceDriver {
            driver_common: DriverCommonData::default(),
            kobj_common: KObjectCommonData::default(),
        };
        Arc::new(Self {
            inner: SpinLock::new(inner),
            kobj_state: LockedKObjectState::default(),
        })
    }

    fn inner(&'_ self) -> SpinLockGuard<'_, InnerLoopDeviceDriver> {
        self.inner.lock()
    }
}

impl Driver for LoopDeviceDriver {
    fn id_table(&self) -> Option<IdTable> {
        Some(IdTable::new(LOOP_BASENAME.to_string(), None))
    }

    fn devices(&self) -> Vec<Arc<dyn Device>> {
        self.inner().driver_common.devices.clone()
    }

    fn add_device(&self, device: Arc<dyn Device>) {
        self.inner().driver_common.push_device(device);
    }

    fn delete_device(&self, device: &Arc<dyn Device>) {
        self.inner().driver_common.delete_device(device);
    }

    fn bus(&self) -> Option<Weak<dyn Bus>> {
        self.inner().driver_common.bus.clone()
    }

    fn set_bus(&self, bus: Option<Weak<dyn Bus>>) {
        self.inner().driver_common.bus = bus;
    }
}

impl KObject for LoopDeviceDriver {
    fn as_any_ref(&self) -> &dyn Any {
        self
    }

    fn set_inode(&self, inode: Option<Arc<KernFSInode>>) {
        self.inner().kobj_common.kern_inode = inode;
    }

    fn inode(&self) -> Option<Arc<KernFSInode>> {
        self.inner().kobj_common.kern_inode.clone()
    }

    fn parent(&self) -> Option<Weak<dyn KObject>> {
        self.inner().kobj_common.parent.clone()
    }

    fn set_parent(&self, parent: Option<Weak<dyn KObject>>) {
        self.inner().kobj_common.parent = parent;
    }

    fn kset(&self) -> Option<Arc<KSet>> {
        self.inner().kobj_common.kset.clone()
    }

    fn set_kset(&self, kset: Option<Arc<KSet>>) {
        self.inner().kobj_common.kset = kset;
    }

    fn kobj_type(&self) -> Option<&'static dyn KObjType> {
        self.inner().kobj_common.kobj_type
    }

    fn set_kobj_type(&self, ktype: Option<&'static dyn KObjType>) {
        self.inner().kobj_common.kobj_type = ktype;
    }

    fn name(&self) -> String {
        LOOP_BASENAME.to_string()
    }

    fn set_name(&self, _name: String) {
        // do nothing
    }

    fn kobj_state(&'_ self) -> RwLockReadGuard<'_, KObjectState> {
        self.kobj_state.read()
    }

    fn kobj_state_mut(&'_ self) -> RwLockWriteGuard<'_, KObjectState> {
        self.kobj_state.write()
    }

    fn set_kobj_state(&self, state: KObjectState) {
        *self.kobj_state.write() = state;
    }
}
