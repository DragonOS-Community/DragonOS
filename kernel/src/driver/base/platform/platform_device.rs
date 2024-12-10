use alloc::{
    string::{String, ToString},
    sync::{Arc, Weak},
};
use ida::IdAllocator;

use crate::{
    driver::base::{
        class::Class,
        device::{
            bus::{Bus, BusState},
            device_manager,
            driver::Driver,
            Device, DeviceCommonData, DevicePrivateData, DeviceType, IdTable,
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
use system_error::SystemError;

use super::{super::device::DeviceState, platform_bus, platform_bus_device, CompatibleTable};

/// 平台设备id分配器
static PLATFORM_DEVID_IDA: SpinLock<IdAllocator> =
    SpinLock::new(IdAllocator::new(0, i32::MAX as usize).unwrap());

#[inline(always)]
pub fn platform_device_manager() -> &'static PlatformDeviceManager {
    &PlatformDeviceManager
}

/// 没有平台设备id
pub const PLATFORM_DEVID_NONE: i32 = -1;
/// 请求自动分配这个平台设备id
pub const PLATFORM_DEVID_AUTO: i32 = -2;

/// @brief: 实现该trait的设备实例应挂载在platform总线上，
///         同时应该实现Device trait
///
/// ## 注意
///
/// 应当在所有实现这个trait的结构体上方，添加 `#[cast_to([sync] PlatformDevice)]`，
/// 否则运行时将报错“该对象不是PlatformDevice”
pub trait PlatformDevice: Device {
    fn pdev_name(&self) -> &str;
    /// 返回平台设备id，以及这个id是否是自动生成的
    ///
    /// 请注意，如果当前设备还没有id，应该返回
    /// (PLATFORM_DEVID_NONE, false)
    fn pdev_id(&self) -> (i32, bool) {
        (PLATFORM_DEVID_NONE, false)
    }

    /// 设置平台设备id
    fn set_pdev_id(&self, id: i32);
    /// 设置id是否为自动分配
    fn set_pdev_id_auto(&self, id_auto: bool);

    /// @brief: 判断设备是否初始化
    /// @parameter: None
    /// @return: 如果已经初始化，返回true，否则，返回false
    #[allow(dead_code)]
    fn is_initialized(&self) -> bool;

    /// @brief: 设置设备状态
    /// @parameter set_state: 设备状态
    /// @return: None
    fn set_state(&self, set_state: DeviceState);
}

#[derive(Debug)]
pub struct PlatformDeviceManager;

impl PlatformDeviceManager {
    /// platform_device_add - add a platform device to device hierarchy
    pub fn device_add(&self, pdev: Arc<dyn PlatformDevice>) -> Result<(), SystemError> {
        if pdev.dev_parent().is_none() {
            pdev.set_dev_parent(Some(Arc::downgrade(
                &(platform_bus_device() as Arc<dyn Device>),
            )));
        }

        pdev.set_bus(Some(Arc::downgrade(&(platform_bus() as Arc<dyn Bus>))));

        let id = pdev.pdev_id().0;
        match id {
            PLATFORM_DEVID_NONE => {
                pdev.set_name(pdev.pdev_name().to_string());
            }
            PLATFORM_DEVID_AUTO => {
                let id = PLATFORM_DEVID_IDA
                    .lock()
                    .alloc()
                    .ok_or(SystemError::EOVERFLOW)?;
                pdev.set_pdev_id(id as i32);
                pdev.set_pdev_id_auto(true);
                pdev.set_name(format!("{}.{}.auto", pdev.pdev_name(), pdev.pdev_id().0));
            }
            _ => {
                pdev.set_name(format!("{}.{}", pdev.pdev_name(), id));
            }
        }

        // todo: 插入资源： https://code.dragonos.org.cn/xref/linux-6.1.9/drivers/base/platform.c?fi=platform_device_add#691
        let r = device_manager().add_device(pdev.clone() as Arc<dyn Device>);
        if r.is_ok() {
            pdev.set_state(DeviceState::Initialized);
            return Ok(()); // success
        } else {
            // failed
            let pdevid = pdev.pdev_id();
            if pdevid.1 {
                PLATFORM_DEVID_IDA.lock().free(pdevid.0 as usize);
                pdev.set_pdev_id(PLATFORM_DEVID_AUTO);
            }

            return r;
        }
    }
}

#[derive(Debug)]
#[cast_to([sync] Device)]
pub struct PlatformBusDevice {
    inner: SpinLock<InnerPlatformBusDevice>,
    kobj_state: LockedKObjectState,
}

impl PlatformBusDevice {
    /// @brief: 创建一个加锁的platform总线实例
    /// @parameter: None
    /// @return: platform总线实例
    pub fn new(
        data: DevicePrivateData,
        parent: Option<Weak<dyn KObject>>,
    ) -> Arc<PlatformBusDevice> {
        let platform_bus_device = Self {
            inner: SpinLock::new(InnerPlatformBusDevice::new(data)),
            kobj_state: LockedKObjectState::new(None),
        };
        platform_bus_device.set_parent(parent);
        return Arc::new(platform_bus_device);
    }

    /// @brief: 获取总线的匹配表
    /// @parameter: None
    /// @return: platform总线匹配表
    #[inline]
    #[allow(dead_code)]
    fn compatible_table(&self) -> CompatibleTable {
        CompatibleTable::new(vec!["platform"])
    }

    /// @brief: 判断总线是否初始化
    /// @parameter: None
    /// @return: 已初始化，返回true，否则，返回false
    #[inline]
    #[allow(dead_code)]
    fn is_initialized(&self) -> bool {
        let state = self.inner.lock().state;
        matches!(state, BusState::Initialized)
    }

    /// @brief: 设置总线状态
    /// @parameter set_state: 总线状态BusState
    /// @return: None
    #[inline]
    #[allow(dead_code)]
    fn set_state(&self, set_state: BusState) {
        let state = &mut self.inner.lock().state;
        *state = set_state;
    }

    /// @brief: 获取总线状态
    /// @parameter: None
    /// @return: 总线状态
    #[inline]
    #[allow(dead_code)]
    fn get_state(&self) -> BusState {
        let state = self.inner.lock().state;
        return state;
    }

    fn inner(&self) -> SpinLockGuard<InnerPlatformBusDevice> {
        self.inner.lock()
    }
}

#[allow(dead_code)]
#[derive(Debug)]
pub struct InnerPlatformBusDevice {
    name: String,
    data: DevicePrivateData,
    state: BusState, // 总线状态
    kobject_common: KObjectCommonData,
    device_common: DeviceCommonData,
}

impl InnerPlatformBusDevice {
    pub fn new(data: DevicePrivateData) -> Self {
        Self {
            data,
            name: "platform".to_string(),
            state: BusState::NotInitialized,
            kobject_common: KObjectCommonData::default(),
            device_common: DeviceCommonData::default(),
        }
    }
}

impl KObject for PlatformBusDevice {
    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn parent(&self) -> Option<Weak<dyn KObject>> {
        self.inner().kobject_common.parent.clone()
    }

    fn inode(&self) -> Option<Arc<KernFSInode>> {
        self.inner().kobject_common.kern_inode.clone()
    }

    fn set_inode(&self, inode: Option<Arc<KernFSInode>>) {
        self.inner().kobject_common.kern_inode = inode;
    }

    fn kobj_type(&self) -> Option<&'static dyn KObjType> {
        self.inner().kobject_common.kobj_type
    }

    fn set_kobj_type(&self, ktype: Option<&'static dyn KObjType>) {
        self.inner().kobject_common.kobj_type = ktype;
    }

    fn kset(&self) -> Option<Arc<KSet>> {
        self.inner().kobject_common.kset.clone()
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

    fn name(&self) -> String {
        self.inner().name.clone()
    }

    fn set_name(&self, name: String) {
        self.inner().name = name;
    }

    fn set_kset(&self, kset: Option<Arc<KSet>>) {
        self.inner().kobject_common.kset = kset;
    }

    fn set_parent(&self, parent: Option<Weak<dyn KObject>>) {
        self.inner().kobject_common.parent = parent;
    }
}

/// @brief: 为Platform实现Device trait，platform总线也是一种设备，属于总线设备类型
impl Device for PlatformBusDevice {
    #[inline]
    #[allow(dead_code)]
    fn dev_type(&self) -> DeviceType {
        return DeviceType::Bus;
    }

    #[inline]
    fn id_table(&self) -> IdTable {
        IdTable::new("platform".to_string(), None)
    }

    fn bus(&self) -> Option<Weak<dyn Bus>> {
        self.inner().device_common.bus.clone()
    }

    fn set_bus(&self, bus: Option<Weak<dyn Bus>>) {
        self.inner().device_common.bus = bus;
    }

    fn driver(&self) -> Option<Arc<dyn Driver>> {
        self.inner().device_common.driver.clone()?.upgrade()
    }

    #[inline]
    fn is_dead(&self) -> bool {
        false
    }

    fn set_driver(&self, driver: Option<Weak<dyn Driver>>) {
        self.inner().device_common.driver = driver;
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

    fn set_class(&self, class: Option<Weak<dyn Class>>) {
        self.inner().device_common.class = class;
    }

    fn dev_parent(&self) -> Option<Weak<dyn Device>> {
        self.inner().device_common.get_parent_weak_or_clear()
    }

    fn set_dev_parent(&self, dev_parent: Option<Weak<dyn Device>>) {
        self.inner().device_common.parent = dev_parent;
    }
}
