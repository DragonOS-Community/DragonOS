use core::any::Any;

use alloc::{
    string::{String, ToString},
    sync::{Arc, Weak},
};

use crate::{
    driver::acpi::acpi_manager,
    filesystem::kernfs::KernFSInode,
    libs::rwlock::{RwLock, RwLockReadGuard, RwLockWriteGuard},
};

use system_error::SystemError;

use super::{
    class::Class,
    device::{
        bus::{subsystem_manager, Bus},
        driver::Driver,
        Device, DeviceCommonData, DeviceType, IdTable,
    },
    kobject::{KObjType, KObject, KObjectCommonData, KObjectState, LockedKObjectState},
    kset::KSet,
    subsys::SubSysPrivate,
};

#[inline(always)]
pub fn cpu_device_manager() -> &'static CpuDeviceManager {
    return &CpuDeviceManager;
}

#[derive(Debug)]
pub struct CpuDeviceManager;

impl CpuDeviceManager {
    /// 初始化设备驱动模型的CPU子系统
    ///
    /// 参考 https://code.dragonos.org.cn/xref/linux-6.1.9/drivers/base/cpu.c?fi=get_cpu_device#622
    pub fn init(&self) -> Result<(), SystemError> {
        let cpu_subsys = CpuSubSystem::new();
        let root_device = CpuSubSystemFakeRootDevice::new();
        subsystem_manager()
            .subsys_system_register(
                &(cpu_subsys as Arc<dyn Bus>),
                &(root_device as Arc<dyn Device>),
            )
            .expect("register cpu subsys failed");

        return Ok(());
    }
}

/// cpu子系统
///
/// 参考 https://code.dragonos.org.cn/xref/linux-6.1.9/drivers/base/cpu.c?fi=get_cpu_device#128
#[derive(Debug)]
struct CpuSubSystem {
    subsys_private: SubSysPrivate,
}

impl CpuSubSystem {
    pub fn new() -> Arc<Self> {
        let bus = Arc::new(Self {
            subsys_private: SubSysPrivate::new("cpu".to_string(), None, None, &[]),
        });
        bus.subsystem()
            .set_bus(Some(Arc::downgrade(&(bus.clone() as Arc<dyn Bus>))));
        return bus;
    }
}

impl Bus for CpuSubSystem {
    fn name(&self) -> String {
        "cpu".to_string()
    }

    fn dev_name(&self) -> String {
        self.name()
    }

    fn remove(&self, _device: &Arc<dyn Device>) -> Result<(), SystemError> {
        todo!()
    }

    fn shutdown(&self, _device: &Arc<dyn Device>) {
        todo!()
    }

    fn resume(&self, _device: &Arc<dyn Device>) -> Result<(), SystemError> {
        todo!()
    }

    fn match_device(
        &self,
        device: &Arc<dyn Device>,
        driver: &Arc<dyn Driver>,
    ) -> Result<bool, SystemError> {
        // ACPI style match is the only one that may succeed.
        return acpi_manager().driver_match_device(driver, device);
    }

    fn subsystem(&self) -> &SubSysPrivate {
        &self.subsys_private
    }
}

#[derive(Debug)]
#[cast_to([sync] Device)]
pub struct CpuSubSystemFakeRootDevice {
    inner: RwLock<InnerCpuSubSystemFakeRootDevice>,
    kobj_state: LockedKObjectState,
}

impl CpuSubSystemFakeRootDevice {
    pub fn new() -> Arc<Self> {
        return Arc::new(Self {
            inner: RwLock::new(InnerCpuSubSystemFakeRootDevice::new()),
            kobj_state: LockedKObjectState::new(None),
        });
    }
}

#[derive(Debug)]
struct InnerCpuSubSystemFakeRootDevice {
    kobject_common: KObjectCommonData,
    device_common: DeviceCommonData,
    name: String,
}

impl InnerCpuSubSystemFakeRootDevice {
    pub fn new() -> Self {
        return Self {
            kobject_common: KObjectCommonData::default(),
            device_common: DeviceCommonData::default(),
            name: "".to_string(),
        };
    }
}

impl Device for CpuSubSystemFakeRootDevice {
    fn dev_type(&self) -> DeviceType {
        todo!()
    }

    fn id_table(&self) -> IdTable {
        IdTable::new("cpu".to_string(), None)
    }

    fn set_bus(&self, bus: Option<Weak<dyn Bus>>) {
        self.inner.write().device_common.bus = bus;
    }

    fn bus(&self) -> Option<Weak<dyn Bus>> {
        self.inner.read().device_common.bus.clone()
    }

    fn driver(&self) -> Option<Arc<dyn Driver>> {
        None
    }

    fn set_driver(&self, _driver: Option<Weak<dyn Driver>>) {
        todo!()
    }

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
        true
    }

    fn set_class(&self, _class: Option<Weak<dyn Class>>) {
        todo!()
    }

    fn dev_parent(&self) -> Option<Weak<dyn Device>> {
        self.inner.read().device_common.parent.clone()
    }

    fn set_dev_parent(&self, dev_parent: Option<Weak<dyn Device>>) {
        self.inner.write().device_common.parent = dev_parent;
    }
}

impl KObject for CpuSubSystemFakeRootDevice {
    fn as_any_ref(&self) -> &dyn Any {
        self
    }

    fn set_inode(&self, inode: Option<Arc<KernFSInode>>) {
        self.inner.write().kobject_common.kern_inode = inode;
    }

    fn inode(&self) -> Option<Arc<KernFSInode>> {
        self.inner.read().kobject_common.kern_inode.clone()
    }

    fn parent(&self) -> Option<Weak<dyn KObject>> {
        self.inner.read().kobject_common.parent.clone()
    }

    fn set_parent(&self, parent: Option<Weak<dyn KObject>>) {
        self.inner.write().kobject_common.parent = parent;
    }

    fn kset(&self) -> Option<Arc<KSet>> {
        self.inner.read().kobject_common.kset.clone()
    }

    fn set_kset(&self, kset: Option<Arc<KSet>>) {
        self.inner.write().kobject_common.kset = kset;
    }

    fn kobj_type(&self) -> Option<&'static dyn KObjType> {
        self.inner.read().kobject_common.kobj_type
    }

    fn set_kobj_type(&self, ktype: Option<&'static dyn KObjType>) {
        self.inner.write().kobject_common.kobj_type = ktype;
    }

    fn name(&self) -> String {
        self.inner.read().name.clone()
    }

    fn set_name(&self, name: String) {
        self.inner.write().name = name;
    }

    fn kobj_state(&self) -> RwLockReadGuard<KObjectState> {
        self.kobj_state.read()
    }

    fn kobj_state_mut(&self) -> RwLockWriteGuard<KObjectState> {
        self.kobj_state.write()
    }

    fn set_kobj_state(&self, state: KObjectState) {
        *self.kobj_state_mut() = state;
    }
}
