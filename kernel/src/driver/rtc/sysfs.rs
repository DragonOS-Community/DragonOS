use alloc::{
    string::String,
    sync::{Arc, Weak},
};
use ida::IdAllocator;
use system_error::SystemError;

use crate::{
    driver::base::{
        class::Class,
        device::{
            bus::Bus, device_manager, driver::Driver, Device, DeviceCommonData, DeviceType, IdTable,
        },
        kobject::{KObjType, KObject, KObjectCommonData, KObjectState, LockedKObjectState},
        kset::KSet,
    },
    filesystem::{
        kernfs::KernFSInode,
        sysfs::{
            file::sysfs_emit_str, Attribute, AttributeGroup, SysFSOpsSupport, SYSFS_ATTR_MODE_RO,
        },
        vfs::syscall::ModeType,
    },
    libs::{
        rwlock::{RwLockReadGuard, RwLockWriteGuard},
        spinlock::{SpinLock, SpinLockGuard},
    },
};

use super::{
    class::sys_class_rtc_instance,
    interface::rtc_read_time,
    utils::{kobj2rtc_device, kobj2rtc_general_device},
    GeneralRtcPriority, RtcClassOps, RtcDevice,
};

static RTC_GENERAL_DEVICE_IDA: SpinLock<IdAllocator> =
    SpinLock::new(IdAllocator::new(0, usize::MAX).unwrap());

pub(super) const RTC_HCTOSYS_DEVICE: &str = "rtc0";

#[derive(Debug)]
#[cast_to([sync] KObject, Device, RtcDevice)]
pub struct RtcGeneralDevice {
    name: String,
    id: usize,
    inner: SpinLock<InnerRtcGeneralDevice>,
    kobj_state: LockedKObjectState,
    priority: GeneralRtcPriority,
}

#[derive(Debug)]
struct InnerRtcGeneralDevice {
    device_common: DeviceCommonData,
    kobject_common: KObjectCommonData,

    class_ops: Option<&'static dyn RtcClassOps>,
    /// 上一次调用`rtc_hctosys()`把时间同步到timekeeper的时候的返回值
    hc2sysfs_result: Result<(), SystemError>,
}

impl RtcGeneralDevice {
    /// 创建一个新的通用RTC设备实例
    ///
    /// 注意，由于还需要进行其他的初始化操作，因此这个函数并不是公开的构造函数。
    fn new(priority: GeneralRtcPriority) -> Arc<Self> {
        let id = RTC_GENERAL_DEVICE_IDA.lock().alloc().unwrap();
        let name = format!("rtc{}", id);
        Arc::new(Self {
            name,
            id,
            inner: SpinLock::new(InnerRtcGeneralDevice {
                device_common: DeviceCommonData::default(),
                kobject_common: KObjectCommonData::default(),
                class_ops: None,
                hc2sysfs_result: Err(SystemError::ENODEV),
            }),
            kobj_state: LockedKObjectState::new(None),
            priority,
        })
    }

    fn inner(&self) -> SpinLockGuard<InnerRtcGeneralDevice> {
        self.inner.lock()
    }

    pub fn set_class_ops(&self, class_ops: &'static dyn RtcClassOps) {
        self.inner().class_ops = Some(class_ops);
    }

    pub fn class_ops(&self) -> Option<&'static dyn RtcClassOps> {
        self.inner().class_ops
    }

    pub fn priority(&self) -> GeneralRtcPriority {
        self.priority
    }

    pub(super) fn set_hc2sys_result(&self, val: Result<(), SystemError>) {
        self.inner().hc2sysfs_result = val;
    }

    pub(super) fn hc2sysfs_result(&self) -> Result<(), SystemError> {
        self.inner().hc2sysfs_result.clone()
    }
}

impl Drop for RtcGeneralDevice {
    fn drop(&mut self) {
        RTC_GENERAL_DEVICE_IDA.lock().free(self.id);
    }
}

impl RtcDevice for RtcGeneralDevice {
    fn class_ops(&self) -> &'static dyn super::RtcClassOps {
        todo!()
    }
}

impl Device for RtcGeneralDevice {
    fn dev_type(&self) -> DeviceType {
        DeviceType::Rtc
    }

    fn id_table(&self) -> IdTable {
        IdTable::new(self.name.clone(), None)
    }

    fn set_bus(&self, bus: Option<Weak<dyn Bus>>) {
        self.inner().device_common.bus = bus;
    }

    fn bus(&self) -> Option<Weak<dyn Bus>> {
        self.inner().device_common.get_bus_weak_or_clear()
    }

    fn set_class(&self, class: Option<Weak<dyn Class>>) {
        self.inner().device_common.class = class;
    }

    fn class(&self) -> Option<Arc<dyn Class>> {
        self.inner()
            .device_common
            .get_class_weak_or_clear()
            .and_then(|x| x.upgrade())
    }

    fn driver(&self) -> Option<Arc<dyn Driver>> {
        self.inner()
            .device_common
            .get_driver_weak_or_clear()
            .and_then(|x| x.upgrade())
    }

    fn set_driver(&self, driver: Option<Weak<dyn Driver>>) {
        self.inner().device_common.driver = driver;
    }

    fn is_dead(&self) -> bool {
        false
    }

    fn can_match(&self) -> bool {
        false
    }

    fn set_can_match(&self, _can_match: bool) {
        // do nothing
    }

    fn state_synced(&self) -> bool {
        true
    }
    fn attribute_groups(&self) -> Option<&'static [&'static dyn AttributeGroup]> {
        Some(&[&RtcAttrGroup])
    }

    fn dev_parent(&self) -> Option<Weak<dyn Device>> {
        self.inner().device_common.get_parent_weak_or_clear()
    }

    fn set_dev_parent(&self, dev_parent: Option<Weak<dyn Device>>) {
        self.inner().device_common.parent = dev_parent;
    }
}

impl KObject for RtcGeneralDevice {
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
        *self.kobj_state_mut() = state;
    }
}

///
/// 用于创建一个通用的RTC设备实例。
///
/// ## 参数
///
/// - `real_dev`: 一个对实际RTC设备的引用，这个设备将作为通用RTC设备的父设备。
pub fn rtc_general_device_create(
    real_dev: &Arc<dyn RtcDevice>,
    priority: Option<GeneralRtcPriority>,
) -> Arc<RtcGeneralDevice> {
    let dev = RtcGeneralDevice::new(priority.unwrap_or_default());
    device_manager().device_default_initialize(&(dev.clone() as Arc<dyn Device>));
    dev.set_dev_parent(Some(Arc::downgrade(real_dev) as Weak<dyn Device>));
    dev.set_class(Some(Arc::downgrade(
        &(sys_class_rtc_instance().cloned().unwrap() as Arc<dyn Class>),
    )));

    return dev;
}

#[derive(Debug)]
struct RtcAttrGroup;

impl AttributeGroup for RtcAttrGroup {
    fn name(&self) -> Option<&str> {
        None
    }

    fn attrs(&self) -> &[&'static dyn Attribute] {
        &[&AttrName, &AttrDate, &AttrTime, &AttrHcToSys]
    }

    fn is_visible(
        &self,
        _kobj: Arc<dyn KObject>,
        attr: &'static dyn Attribute,
    ) -> Option<ModeType> {
        // todo: https://code.dragonos.org.cn/xref/linux-6.6.21/drivers/rtc/sysfs.c#280

        return Some(attr.mode());
    }
}

#[derive(Debug)]
struct AttrName;

impl Attribute for AttrName {
    fn name(&self) -> &str {
        "name"
    }

    fn mode(&self) -> ModeType {
        SYSFS_ATTR_MODE_RO
    }

    fn support(&self) -> SysFSOpsSupport {
        SysFSOpsSupport::ATTR_SHOW
    }

    fn show(&self, kobj: Arc<dyn KObject>, buf: &mut [u8]) -> Result<usize, SystemError> {
        let rtc_device = kobj
            .parent()
            .and_then(|x| x.upgrade())
            .ok_or(SystemError::ENODEV)?;
        let rtc_device = kobj2rtc_device(rtc_device).ok_or(SystemError::EINVAL)?;

        let driver_name = rtc_device.driver().ok_or(SystemError::ENODEV)?.name();
        let device_name = rtc_device.name();
        sysfs_emit_str(buf, &format!("{} {}\n", driver_name, device_name))
    }
}

#[derive(Debug)]
struct AttrDate;

impl Attribute for AttrDate {
    fn name(&self) -> &str {
        "date"
    }

    fn mode(&self) -> ModeType {
        SYSFS_ATTR_MODE_RO
    }

    fn support(&self) -> SysFSOpsSupport {
        SysFSOpsSupport::ATTR_SHOW
    }

    fn show(&self, kobj: Arc<dyn KObject>, buf: &mut [u8]) -> Result<usize, SystemError> {
        let rtc_device: Arc<RtcGeneralDevice> =
            kobj2rtc_general_device(kobj).ok_or(SystemError::EINVAL)?;
        let time = rtc_read_time(&rtc_device)?;
        sysfs_emit_str(buf, &time.date_string())
    }
}

#[derive(Debug)]
struct AttrTime;

impl Attribute for AttrTime {
    fn name(&self) -> &str {
        "time"
    }

    fn mode(&self) -> ModeType {
        SYSFS_ATTR_MODE_RO
    }

    fn support(&self) -> SysFSOpsSupport {
        SysFSOpsSupport::ATTR_SHOW
    }

    fn show(&self, kobj: Arc<dyn KObject>, buf: &mut [u8]) -> Result<usize, SystemError> {
        let rtc_device = kobj2rtc_general_device(kobj).ok_or(SystemError::EINVAL)?;
        let time = rtc_read_time(&rtc_device)?;
        sysfs_emit_str(buf, &time.time_string())
    }
}

#[derive(Debug)]
struct AttrHcToSys;

impl Attribute for AttrHcToSys {
    fn name(&self) -> &str {
        "hctosys"
    }

    fn mode(&self) -> ModeType {
        SYSFS_ATTR_MODE_RO
    }

    fn support(&self) -> SysFSOpsSupport {
        SysFSOpsSupport::ATTR_SHOW
    }

    fn show(&self, kobj: Arc<dyn KObject>, buf: &mut [u8]) -> Result<usize, SystemError> {
        let rtc_device = kobj2rtc_general_device(kobj).ok_or(SystemError::EINVAL)?;
        if rtc_device.hc2sysfs_result().is_ok() && rtc_device.name().eq(RTC_HCTOSYS_DEVICE) {
            return sysfs_emit_str(buf, "1\n");
        }

        return sysfs_emit_str(buf, "0\n");
    }
}
