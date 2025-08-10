use core::any::Any;

use alloc::{
    string::{String, ToString},
    sync::{Arc, Weak},
};
use log::error;
use system_error::SystemError;
use unified_init::macros::unified_init;

use crate::{
    arch::{io::PortIOArch, CurrentIrqArch, CurrentPortIOArch},
    driver::{
        base::{
            class::Class,
            device::{
                bus::Bus, device_manager, driver::Driver, Device, DeviceCommonData, DeviceState,
                DeviceType, IdTable,
            },
            kobject::{KObjType, KObject, KObjectCommonData, KObjectState, LockedKObjectState},
            kset::KSet,
            platform::platform_device::{platform_device_manager, PlatformDevice},
        },
        rtc::{RtcClassOps, RtcDevice, RtcTime},
    },
    exception::InterruptArch,
    filesystem::kernfs::KernFSInode,
    init::initcall::INITCALL_DEVICE,
    libs::{
        mutex::Mutex,
        rwlock::{RwLockReadGuard, RwLockWriteGuard},
        spinlock::{SpinLock, SpinLockGuard},
    },
};

#[derive(Debug)]
#[cast_to([sync] Device, PlatformDevice, RtcDevice)]
struct CmosRtcDevice {
    inner: SpinLock<InnerCmosRtc>,
    locked_kobjstate: LockedKObjectState,
    ops_mutex: Mutex<()>,
}

impl CmosRtcDevice {
    const NAME: &str = "rtc_cmos";
    pub fn new() -> Arc<Self> {
        let r = CmosRtcDevice {
            inner: SpinLock::new(InnerCmosRtc {
                device_common: DeviceCommonData::default(),
                kobject_common: KObjectCommonData::default(),
                device_state: DeviceState::NotInitialized,
            }),
            locked_kobjstate: LockedKObjectState::new(None),
            ops_mutex: Mutex::new(()),
        };

        r.inner().device_common.can_match = true;

        Arc::new(r)
    }

    fn inner(&self) -> SpinLockGuard<InnerCmosRtc> {
        self.inner.lock()
    }

    ///置位0x70的第7位，禁止不可屏蔽中断
    #[inline]
    fn read_cmos(&self, addr: u8) -> u8 {
        unsafe {
            CurrentPortIOArch::out8(0x70, 0x80 | addr);
            return CurrentPortIOArch::in8(0x71);
        }
    }
}

#[derive(Debug)]
struct InnerCmosRtc {
    device_common: DeviceCommonData,
    kobject_common: KObjectCommonData,

    device_state: DeviceState,
}

impl RtcDevice for CmosRtcDevice {
    fn class_ops(&self) -> &'static dyn RtcClassOps {
        &CmosRtcClassOps
    }
}

impl PlatformDevice for CmosRtcDevice {
    fn pdev_name(&self) -> &str {
        Self::NAME
    }

    fn set_pdev_id(&self, _id: i32) {
        todo!()
    }

    fn set_pdev_id_auto(&self, _id_auto: bool) {
        todo!()
    }

    fn is_initialized(&self) -> bool {
        self.inner().device_state == DeviceState::Initialized
    }

    fn set_state(&self, set_state: DeviceState) {
        self.inner().device_state = set_state;
    }
}

impl Device for CmosRtcDevice {
    fn dev_type(&self) -> DeviceType {
        DeviceType::Rtc
    }

    fn id_table(&self) -> IdTable {
        IdTable::new(Self::NAME.to_string(), None)
    }

    fn set_bus(&self, bus: Option<Weak<dyn Bus>>) {
        self.inner().device_common.bus = bus;
    }

    fn set_class(&self, class: Option<Weak<dyn Class>>) {
        self.inner().device_common.class = class;
    }

    fn class(&self) -> Option<Arc<dyn Class>> {
        self.inner()
            .device_common
            .get_class_weak_or_clear()
            .and_then(|c| c.upgrade())
    }

    fn driver(&self) -> Option<Arc<dyn Driver>> {
        self.inner()
            .device_common
            .get_driver_weak_or_clear()
            .and_then(|d| d.upgrade())
    }

    fn set_driver(&self, driver: Option<Weak<dyn Driver>>) {
        self.inner().device_common.driver = driver;
    }

    fn is_dead(&self) -> bool {
        self.inner().device_common.dead
    }

    fn can_match(&self) -> bool {
        self.inner().device_common.can_match
    }

    fn set_can_match(&self, can_match: bool) {
        self.inner().device_common.can_match = can_match;
    }

    fn state_synced(&self) -> bool {
        true
    }

    fn bus(&self) -> Option<Weak<dyn Bus>> {
        self.inner().device_common.get_bus_weak_or_clear()
    }

    fn dev_parent(&self) -> Option<Weak<dyn Device>> {
        self.inner().device_common.get_parent_weak_or_clear()
    }

    fn set_dev_parent(&self, parent: Option<Weak<dyn Device>>) {
        self.inner().device_common.parent = parent;
    }
}

impl KObject for CmosRtcDevice {
    fn as_any_ref(&self) -> &dyn Any {
        self
    }

    fn set_inode(&self, inode: Option<Arc<KernFSInode>>) {
        self.inner().kobject_common.kern_inode = inode;
    }

    fn inode(&self) -> Option<Arc<KernFSInode>> {
        self.inner().kobject_common.kern_inode.clone()
    }

    fn parent(&self) -> Option<Weak<dyn KObject>> {
        self.inner().kobject_common.get_parent_or_clear_weak()
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
        // Do nothing
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

#[derive(Debug)]
pub struct CmosRtcClassOps;

impl RtcClassOps for CmosRtcClassOps {
    fn read_time(&self, dev: &Arc<dyn RtcDevice>) -> Result<RtcTime, SystemError> {
        // 检查是否为cmos rtc
        let dev = dev
            .as_any_ref()
            .downcast_ref::<CmosRtcDevice>()
            .ok_or(SystemError::EINVAL)?;

        let _guard = dev.ops_mutex.lock();

        // 为防止中断请求打断该过程，需要先关中断
        let irq_guard = unsafe { CurrentIrqArch::save_and_disable_irq() };
        //0x0B
        let status_register_b: u8 = dev.read_cmos(0x0B); // 读取状态寄存器B
        let is_24h: bool = (status_register_b & 0x02) != 0;
        // 判断是否启用24小时模式
        let is_binary: bool = (status_register_b & 0x04) != 0; // 判断是否为二进制码
        let mut res = RtcTime::default();

        loop {
            res.year = dev.read_cmos(CMOSTimeSelector::Year as u8) as i32;
            res.month = dev.read_cmos(CMOSTimeSelector::Month as u8) as i32;
            res.mday = dev.read_cmos(CMOSTimeSelector::Day as u8) as i32;
            res.hour = dev.read_cmos(CMOSTimeSelector::Hour as u8) as i32;
            res.minute = dev.read_cmos(CMOSTimeSelector::Minute as u8) as i32;
            res.second = dev.read_cmos(CMOSTimeSelector::Second as u8) as i32;

            if res.second == dev.read_cmos(CMOSTimeSelector::Second as u8) as i32 {
                break;
            } // 若读取时间过程中时间发生跳变则重新读取
        }

        unsafe {
            CurrentPortIOArch::out8(0x70, 0x00);
        }

        if !is_binary
        // 把BCD转为二进制
        {
            res.second = (res.second & 0xf) + (res.second >> 4) * 10;
            res.minute = (res.minute & 0xf) + (res.minute >> 4) * 10;
            res.hour = ((res.hour & 0xf) + ((res.hour & 0x70) >> 4) * 10) | (res.hour & 0x80);
            res.mday = (res.mday & 0xf) + ((res.mday / 16) * 10);
            res.month = (res.month & 0xf) + (res.month >> 4) * 10;
            res.year = (res.year & 0xf) + (res.year >> 4) * 10;
        }
        res.year += 100;

        if (!is_24h) && (res.hour & 0x80) != 0 {
            res.hour = ((res.hour & 0x7f) + 12) % 24;
        } // 将十二小时制转为24小时

        res.month -= 1;

        drop(irq_guard);

        return Ok(res);
    }

    fn set_time(&self, _dev: &Arc<dyn RtcDevice>, _time: &RtcTime) -> Result<(), SystemError> {
        error!("set_time is not implemented for CmosRtcClassOps");
        Err(SystemError::ENOSYS)
    }
}

/// used in the form of u8
#[repr(u8)]
enum CMOSTimeSelector {
    Second = 0x00,
    Minute = 0x02,
    Hour = 0x04,
    Day = 0x07,
    Month = 0x08,
    Year = 0x09,
}

#[unified_init(INITCALL_DEVICE)]
pub fn cmos_rtc_device_init() -> Result<(), SystemError> {
    let device = CmosRtcDevice::new();
    device_manager().device_default_initialize(&(device.clone() as Arc<dyn Device>));
    platform_device_manager().device_add(device)?;

    return Ok(());
}
