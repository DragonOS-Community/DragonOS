//! Shared sysfs device for DragonOS software probe PMUs.

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
use crate::filesystem::vfs::InodeMode;
use crate::libs::rwsem::{RwSemReadGuard, RwSemWriteGuard};
use crate::libs::spinlock::{SpinLock, SpinLockGuard};
use alloc::string::{String, ToString};
use alloc::sync::{Arc, Weak};
use core::fmt::Debug;
use system_error::SystemError;

#[derive(Debug)]
#[cast_to([sync] Device)]
pub struct ProbePmuDevice {
    inner: SpinLock<InnerProbePmuDevice>,
    kobj_state: LockedKObjectState,
    name: String,
    pmu_type: u32,
}

#[derive(Debug)]
struct InnerProbePmuDevice {
    kobject_common: KObjectCommonData,
    device_common: DeviceCommonData,
}

impl ProbePmuDevice {
    pub fn new(name: &str, pmu_type: u32, parent: Option<Weak<dyn KObject>>) -> Arc<Self> {
        let bus_device = Self {
            inner: SpinLock::new(InnerProbePmuDevice {
                kobject_common: KObjectCommonData::default(),
                device_common: DeviceCommonData::default(),
            }),
            kobj_state: LockedKObjectState::new(None),
            name: name.to_string(),
            pmu_type,
        };
        bus_device.set_parent(parent);
        return Arc::new(bus_device);
    }

    fn inner(&self) -> SpinLockGuard<'_, InnerProbePmuDevice> {
        self.inner.lock()
    }

    fn pmu_type(&self) -> u32 {
        self.pmu_type
    }
}

impl KObject for ProbePmuDevice {
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

    fn kobj_state(&self) -> RwSemReadGuard<'_, KObjectState> {
        self.kobj_state.read()
    }

    fn kobj_state_mut(&self) -> RwSemWriteGuard<'_, KObjectState> {
        self.kobj_state.write()
    }

    fn set_kobj_state(&self, state: KObjectState) {
        *self.kobj_state.write() = state;
    }
}

impl Device for ProbePmuDevice {
    #[inline]
    #[allow(dead_code)]
    fn dev_type(&self) -> DeviceType {
        return DeviceType::Other;
    }

    #[inline]
    fn id_table(&self) -> IdTable {
        IdTable::new(self.name.clone(), None)
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
pub struct ProbeTypeAttr;

impl Attribute for ProbeTypeAttr {
    fn name(&self) -> &str {
        "type"
    }

    fn mode(&self) -> InodeMode {
        InodeMode::S_IRUGO
    }

    fn support(&self) -> SysFSOpsSupport {
        SysFSOpsSupport::ATTR_SHOW
    }
    fn show(&self, kobj: Arc<dyn KObject>, buf: &mut [u8]) -> Result<usize, SystemError> {
        let device = kobj
            .as_any_ref()
            .downcast_ref::<ProbePmuDevice>()
            .ok_or(SystemError::EINVAL)?;
        let value = alloc::format!("{}\n", device.pmu_type());
        if buf.len() < value.len() {
            return Err(SystemError::EINVAL);
        }
        buf[..value.len()].copy_from_slice(value.as_bytes());
        Ok(value.len())
    }
}

pub static PROBE_TYPE_ATTR: ProbeTypeAttr = ProbeTypeAttr;
