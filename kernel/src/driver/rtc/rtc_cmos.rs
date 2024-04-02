use alloc::{
    string::{String, ToString},
    sync::{Arc, Weak},
    vec::Vec,
};
use intertrait::cast::CastArc;
use system_error::SystemError;
use unified_init::macros::unified_init;

use super::{
    class::rtc_register_device,
    sysfs::{rtc_general_device_create, RtcGeneralDevice},
    RtcClassOps, RtcDevice, RtcTime,
};
use crate::{
    driver::base::{
        device::{
            bus::Bus,
            driver::{Driver, DriverCommonData},
            Device, IdTable,
        },
        kobject::{KObjType, KObject, KObjectCommonData, KObjectState, LockedKObjectState},
        kset::KSet,
        platform::{
            platform_device::PlatformDevice,
            platform_driver::{platform_driver_manager, PlatformDriver},
        },
    },
    filesystem::kernfs::KernFSInode,
    init::initcall::INITCALL_DEVICE,
    libs::{
        rwlock::{RwLock, RwLockReadGuard, RwLockWriteGuard},
        spinlock::{SpinLock, SpinLockGuard},
    },
};

static CMOS_RTC_GENERAL_DEVICE: RwLock<Option<Arc<RtcGeneralDevice>>> = RwLock::new(None);

#[derive(Debug)]
#[cast_to([sync] Driver, PlatformDriver)]
struct CmosPlatformDriver {
    inner: SpinLock<InnerCmosPlatformDriver>,
    locked_kobjstate: LockedKObjectState,
}

impl CmosPlatformDriver {
    const NAME: &str = "rtc_cmos";

    fn new() -> Arc<Self> {
        Arc::new(CmosPlatformDriver {
            inner: SpinLock::new(InnerCmosPlatformDriver {
                driver_common: DriverCommonData::default(),
                kobject_common: KObjectCommonData::default(),
            }),
            locked_kobjstate: LockedKObjectState::new(None),
        })
    }

    fn inner(&self) -> SpinLockGuard<InnerCmosPlatformDriver> {
        self.inner.lock()
    }
}

#[derive(Debug)]
struct InnerCmosPlatformDriver {
    driver_common: DriverCommonData,
    kobject_common: KObjectCommonData,
}

impl PlatformDriver for CmosPlatformDriver {
    fn probe(&self, device: &Arc<dyn PlatformDevice>) -> Result<(), SystemError> {
        let dev = device
            .clone()
            .arc_any()
            .cast::<dyn RtcDevice>()
            .map_err(|_| SystemError::ENODEV)?;

        if dev.id_table() != self.id_table().unwrap() {
            return Err(SystemError::ENODEV);
        }

        if CMOS_RTC_GENERAL_DEVICE.read().is_some() {
            return Err(SystemError::EBUSY);
        }

        let mut guard = CMOS_RTC_GENERAL_DEVICE.write();

        // 再次检查
        if guard.is_some() {
            return Err(SystemError::EBUSY);
        }

        let general_rtc_device: Arc<RtcGeneralDevice> = rtc_general_device_create(&dev, None);
        guard.replace(general_rtc_device.clone());

        general_rtc_device.set_class_ops(&CmosRtcClassOps);
        drop(guard);

        rtc_register_device(&general_rtc_device)
            .expect("cmos_rtc: register general rtc device failed");

        return Ok(());
    }

    fn remove(&self, _device: &Arc<dyn PlatformDevice>) -> Result<(), SystemError> {
        // todo: remove
        Err(SystemError::ENOSYS)
    }

    fn shutdown(&self, _device: &Arc<dyn PlatformDevice>) -> Result<(), SystemError> {
        unimplemented!("cmos platform driver shutdown")
    }

    fn suspend(&self, _device: &Arc<dyn PlatformDevice>) -> Result<(), SystemError> {
        todo!("cmos platform driver suspend")
    }

    fn resume(&self, _device: &Arc<dyn PlatformDevice>) -> Result<(), SystemError> {
        todo!("cmos platform driver resume")
    }
}

impl Driver for CmosPlatformDriver {
    fn id_table(&self) -> Option<IdTable> {
        Some(IdTable::new(Self::NAME.to_string(), None))
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

    fn set_bus(&self, bus: Option<Weak<dyn Bus>>) {
        self.inner().driver_common.bus = bus;
    }

    fn bus(&self) -> Option<Weak<dyn Bus>> {
        self.inner().driver_common.bus.clone()
    }
}

impl KObject for CmosPlatformDriver {
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
        self.locked_kobjstate.read()
    }

    fn kobj_state_mut(&self) -> RwLockWriteGuard<KObjectState> {
        self.locked_kobjstate.write()
    }

    fn set_kobj_state(&self, state: KObjectState) {
        *self.locked_kobjstate.write() = state;
    }
}

#[unified_init(INITCALL_DEVICE)]
pub fn cmos_rtc_driver_init() -> Result<(), SystemError> {
    let driver = CmosPlatformDriver::new();

    platform_driver_manager().register(driver)?;

    return Ok(());
}

#[derive(Debug)]
struct CmosRtcClassOps;

impl RtcClassOps for CmosRtcClassOps {
    fn read_time(&self, dev: &Arc<dyn RtcDevice>) -> Result<RtcTime, SystemError> {
        dev.class_ops().read_time(dev)
    }

    fn set_time(&self, dev: &Arc<dyn RtcDevice>, time: &RtcTime) -> Result<(), SystemError> {
        dev.class_ops().set_time(dev, time)
    }
}
