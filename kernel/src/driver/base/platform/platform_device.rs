use alloc::{
    string::{String, ToString},
    sync::{Arc, Weak},
};

use crate::{
    driver::{
        base::{
            device::{
                bus::{Bus, BusState},
                Device, DeviceNumber, DevicePrivateData, DeviceType, IdTable,
            },
            kobject::{KObjType, KObject, KObjectState, LockedKObjectState},
            kset::KSet,
        },
        Driver,
    },
    filesystem::kernfs::KernFSInode,
    libs::{
        rwlock::{RwLockReadGuard, RwLockWriteGuard},
        spinlock::SpinLock,
    },
};

use super::{super::device::DeviceState, CompatibleTable};

/// @brief: 实现该trait的设备实例应挂载在platform总线上，
///         同时应该实现Device trait
pub trait PlatformDevice: Device {
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
            kobj_state: LockedKObjectState::new(KObjectState::empty()),
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

    // /// @brief:
    // /// @parameter: None
    // /// @return: 总线状态
    // #[inline]
    // #[allow(dead_code)]
    // fn set_driver(&self, driver: Option<Arc<LockedPlatformBusDriver>>) {
    //     self.0.lock().driver = driver;
    // }
}

/// @brief: platform总线
#[derive(Debug, Clone)]
pub struct InnerPlatformBusDevice {
    name: String,
    data: DevicePrivateData,
    state: BusState,                   // 总线状态
    parent: Option<Weak<dyn KObject>>, // 总线的父对象

    kernfs_inode: Option<Arc<KernFSInode>>,
    /// 当前设备挂载到的总线
    bus: Option<Arc<dyn Bus>>,
    /// 当前设备已经匹配的驱动
    driver: Option<Arc<dyn Driver>>,
}

/// @brief: platform方法集
impl InnerPlatformBusDevice {
    /// @brief: 创建一个platform总线实例
    /// @parameter: None
    /// @return: platform总线实例
    pub fn new(data: DevicePrivateData, parent: Option<Weak<dyn KObject>>) -> Self {
        Self {
            data,
            name: "platform".to_string(),
            state: BusState::NotInitialized,
            parent,
            kernfs_inode: None,
            bus: None,
            driver: None,
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
        None
    }

    fn kset(&self) -> Option<Arc<KSet>> {
        None
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
        todo!()
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
    #[allow(dead_code)]
    fn id_table(&self) -> IdTable {
        IdTable::new("platform".to_string(), DeviceNumber::new(0))
    }

    fn bus(&self) -> Option<Arc<dyn Bus>> {
        self.inner.lock().bus.clone()
    }

    fn driver(&self) -> Option<Arc<dyn Driver>> {
        self.inner.lock().driver.clone()
    }

    #[inline]
    fn is_dead(&self) -> bool {
        false
    }

    fn set_driver(&self, driver: Option<Arc<dyn Driver>>) {
        self.inner.lock().driver = driver;
    }
}
