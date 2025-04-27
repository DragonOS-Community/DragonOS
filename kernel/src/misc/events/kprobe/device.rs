use crate::driver::base::class::Class;
use crate::driver::base::device::bus::Bus;
use crate::driver::base::device::driver::Driver;
use crate::driver::base::device::{Device, DeviceCommonData, DeviceType, IdTable};
use crate::driver::base::kobject::{
    KObjType, KObject, KObjectCommonData, KObjectState, LockedKObjectState,
};
use crate::driver::base::kset::KSet;
use crate::filesystem::kernfs::KernFSInode;
use crate::filesystem::sysfs::{Attribute, SysFSOpsSupport};
use crate::filesystem::vfs::syscall::ModeType;
use crate::libs::rwlock::{RwLockReadGuard, RwLockWriteGuard};
use crate::libs::spinlock::{SpinLock, SpinLockGuard};
use alloc::string::{String, ToString};
use alloc::sync::{Arc, Weak};
use core::fmt::Debug;
use system_error::SystemError;

#[derive(Debug)]
#[cast_to([sync] Device)]
pub struct KprobeDevice {
    inner: SpinLock<InnerKprobeDevice>,
    kobj_state: LockedKObjectState,
    name: String,
}

#[derive(Debug)]
struct InnerKprobeDevice {
    kobject_common: KObjectCommonData,
    device_common: DeviceCommonData,
}

impl KprobeDevice {
    pub fn new(parent: Option<Weak<dyn KObject>>) -> Arc<Self> {
        let bus_device = Self {
            inner: SpinLock::new(InnerKprobeDevice {
                kobject_common: KObjectCommonData::default(),
                device_common: DeviceCommonData::default(),
            }),
            kobj_state: LockedKObjectState::new(None),
            name: "kprobe".to_string(),
        };
        bus_device.set_parent(parent);
        return Arc::new(bus_device);
    }

    fn inner(&self) -> SpinLockGuard<InnerKprobeDevice> {
        self.inner.lock()
    }
}

impl KObject for KprobeDevice {
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
        self.name.clone()
    }

    fn set_name(&self, _name: String) {}

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

impl Device for KprobeDevice {
    #[inline]
    #[allow(dead_code)]
    fn dev_type(&self) -> DeviceType {
        return DeviceType::Other;
    }

    #[inline]
    fn id_table(&self) -> IdTable {
        IdTable::new("kprobe".to_string(), None)
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

    fn driver(&self) -> Option<Arc<dyn Driver>> {
        self.inner().device_common.driver.clone()?.upgrade()
    }

    fn set_driver(&self, driver: Option<Weak<dyn Driver>>) {
        self.inner().device_common.driver = driver;
    }

    #[inline]
    fn is_dead(&self) -> bool {
        false
    }

    fn can_match(&self) -> bool {
        todo!()
    }

    fn set_can_match(&self, _can_match: bool) {
        todo!()
    }

    fn state_synced(&self) -> bool {
        todo!()
    }

    fn dev_parent(&self) -> Option<Weak<dyn Device>> {
        self.inner().device_common.get_parent_weak_or_clear()
    }

    fn set_dev_parent(&self, dev_parent: Option<Weak<dyn Device>>) {
        self.inner().device_common.parent = dev_parent;
    }
}

#[derive(Debug)]
pub struct KprobeAttr;

impl Attribute for KprobeAttr {
    fn name(&self) -> &str {
        "type"
    }

    fn mode(&self) -> ModeType {
        ModeType::S_IRUGO
    }

    fn support(&self) -> SysFSOpsSupport {
        SysFSOpsSupport::ATTR_SHOW
    }
    fn show(&self, _kobj: Arc<dyn KObject>, buf: &mut [u8]) -> Result<usize, SystemError> {
        if buf.is_empty() {
            return Err(SystemError::EINVAL);
        }
        // perf_type_id::PERF_TYPE_MAX
        buf[0] = b'6';
        Ok(1)
    }
}
