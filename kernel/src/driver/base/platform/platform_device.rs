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
            Device, DevicePrivateData, DeviceType, IdTable,
        },
        kobject::{KObjType, KObject, KObjectState, LockedKObjectState},
        kset::KSet,
    },
    filesystem::kernfs::KernFSInode,
    libs::{
        rwlock::{RwLockReadGuard, RwLockWriteGuard},
        spinlock::SpinLock,
    },
};
use system_error::SystemError;

use super::{super::device::DeviceState, platform_bus, platform_bus_device, CompatibleTable};

/// 平台设备id分配器
static PLATFORM_DEVID_IDA: IdAllocator = IdAllocator::new(0, i32::MAX as usize);

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

    fn compatible_table(&self) -> CompatibleTable;
    /// @brief: 判断设备是否初始化
    /// @parameter: None
    /// @return: 如果已经初始化，返回true，否则，返回false
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
        if pdev.parent().is_none() {
            pdev.set_parent(Some(Arc::downgrade(
                &(platform_bus_device() as Arc<dyn KObject>),
            )));
        }

        pdev.set_bus(Some(Arc::downgrade(&(platform_bus() as Arc<dyn Bus>))));

        let id = pdev.pdev_id().0;
        match id {
            PLATFORM_DEVID_NONE => {
                pdev.set_name(format!("{}", pdev.pdev_name()));
            }
            PLATFORM_DEVID_AUTO => {
                let id = PLATFORM_DEVID_IDA.alloc().ok_or(SystemError::EOVERFLOW)?;
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
                PLATFORM_DEVID_IDA.free(pdevid.0 as usize);
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
        return Arc::new(PlatformBusDevice {
            inner: SpinLock::new(InnerPlatformBusDevice::new(data, parent)),
            kobj_state: LockedKObjectState::new(None),
        });
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
        match state {
            BusState::Initialized => true,
            _ => false,
        }
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
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct InnerPlatformBusDevice {
    name: String,
    data: DevicePrivateData,
    state: BusState,                   // 总线状态
    parent: Option<Weak<dyn KObject>>, // 总线的父对象

    kernfs_inode: Option<Arc<KernFSInode>>,
    /// 当前设备挂载到的总线
    bus: Option<Weak<dyn Bus>>,
    /// 当前设备已经匹配的驱动
    driver: Option<Weak<dyn Driver>>,

    ktype: Option<&'static dyn KObjType>,
    kset: Option<Arc<KSet>>,
}

impl InnerPlatformBusDevice {
    pub fn new(data: DevicePrivateData, parent: Option<Weak<dyn KObject>>) -> Self {
        Self {
            data,
            name: "platform".to_string(),
            state: BusState::NotInitialized,
            parent,
            kernfs_inode: None,
            bus: None,
            driver: None,
            ktype: None,
            kset: None,
        }
    }
}

impl KObject for PlatformBusDevice {
    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn parent(&self) -> Option<Weak<dyn KObject>> {
        self.inner.lock().parent.clone()
    }

    fn inode(&self) -> Option<Arc<KernFSInode>> {
        self.inner.lock().kernfs_inode.clone()
    }

    fn set_inode(&self, inode: Option<Arc<KernFSInode>>) {
        self.inner.lock().kernfs_inode = inode;
    }

    fn kobj_type(&self) -> Option<&'static dyn KObjType> {
        self.inner.lock().ktype.clone()
    }

    fn set_kobj_type(&self, ktype: Option<&'static dyn KObjType>) {
        self.inner.lock().ktype = ktype;
    }

    fn kset(&self) -> Option<Arc<KSet>> {
        self.inner.lock().kset.clone()
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
        self.inner.lock().name.clone()
    }

    fn set_name(&self, name: String) {
        self.inner.lock().name = name;
    }

    fn set_kset(&self, kset: Option<Arc<KSet>>) {
        self.inner.lock().kset = kset;
    }

    fn set_parent(&self, parent: Option<Weak<dyn KObject>>) {
        self.inner.lock().parent = parent;
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
        self.inner.lock().bus.clone()
    }

    fn set_bus(&self, bus: Option<Weak<dyn Bus>>) {
        self.inner.lock().bus = bus;
    }

    fn driver(&self) -> Option<Arc<dyn Driver>> {
        self.inner.lock().driver.clone()?.upgrade()
    }

    #[inline]
    fn is_dead(&self) -> bool {
        false
    }

    fn set_driver(&self, driver: Option<Weak<dyn Driver>>) {
        self.inner.lock().driver = driver;
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

    fn set_class(&self, _class: Option<Arc<dyn Class>>) {
        todo!()
    }
}
