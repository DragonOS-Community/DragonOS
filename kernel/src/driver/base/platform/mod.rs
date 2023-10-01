use super::{
    device::{
        bus::{bus_driver_register, bus_register, Bus, BusDriver, BusState},
        driver::DriverError,
        Device, DeviceError, DeviceNumber, DevicePrivateData, DeviceResource, DeviceType, IdTable,
    },
    kobject::{KObjType, KObject, KObjectState},
    kset::KSet,
};
use crate::{
    driver::Driver,
    filesystem::{kernfs::KernFSInode, vfs::IndexNode},
    libs::{
        rwlock::{RwLockReadGuard, RwLockWriteGuard},
        spinlock::SpinLock,
    },
    syscall::SystemError,
};
use alloc::{
    collections::{BTreeMap, BTreeSet},
    string::ToString,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::fmt::Debug;
use platform_device::PlatformDevice;
use platform_driver::PlatformDriver;

pub mod platform_device;
pub mod platform_driver;

/// @brief: platform总线匹配表
///         总线上的设备和驱动都存在一份匹配表
///         根据匹配表条目是否匹配来辨识设备和驱动能否进行匹配
#[derive(Debug, Clone)]
pub struct CompatibleTable(BTreeSet<&'static str>);

/// @brief: 匹配表操作方法集
impl CompatibleTable {
    /// @brief: 创建一个新的匹配表
    /// @parameter id_vec: 匹配条目数组
    /// @return: 匹配表
    #[inline]
    #[allow(dead_code)]
    pub fn new(id_vec: Vec<&'static str>) -> CompatibleTable {
        CompatibleTable(BTreeSet::from_iter(id_vec.iter().cloned()))
    }

    /// @brief: 判断两个匹配表是否能够匹配
    /// @parameter other: 其他匹配表
    /// @return: 如果匹配成功，返回true，否则，返回false
    #[allow(dead_code)]
    pub fn matches(&self, other: &CompatibleTable) -> bool {
        self.0.intersection(&other.0).next().is_some()
    }

    /// @brief: 添加一组匹配条目
    /// @param:
    #[allow(dead_code)]
    pub fn add_device(&mut self, devices: Vec<&'static str>) {
        for str in devices {
            self.0.insert(str);
        }
    }
}

#[derive(Debug)]
pub struct LockedPlatformBusDriver(SpinLock<PlatformBusDriver>);

impl LockedPlatformBusDriver {
    /// @brief: 创建一个platform总线加锁驱动，该驱动用于匹配plaform总线
    /// @parameter: None
    /// @return: platfor总线驱动
    #[inline]
    #[allow(dead_code)]
    pub fn new() -> LockedPlatformBusDriver {
        LockedPlatformBusDriver(SpinLock::new(PlatformBusDriver::new()))
    }

    /// @brief: 获取该驱动的匹配表
    /// @parameter: None
    /// @return: 驱动的匹配表
    #[inline]
    #[allow(dead_code)]
    fn get_compatible_table(&self) -> CompatibleTable {
        CompatibleTable::new(vec!["platform"])
    }

    /// @brief: 根据设备标识符获取platform总线上的设备
    /// @parameter id_table: 设备标识符
    /// @return: 总线上的设备
    #[inline]
    #[allow(dead_code)]
    fn get_device(&self, id_table: &IdTable) -> Option<Arc<dyn PlatformDevice>> {
        let device_map = &self.0.lock().devices;
        return device_map.get(id_table).cloned();
    }

    /// @brief: 根据设备驱动标识符获取platform总线上的驱动
    /// @parameter id_table: 设备驱动标识符
    /// @return: 总线上的驱动
    #[inline]
    #[allow(dead_code)]
    fn get_driver(&self, id_table: &IdTable) -> Option<Arc<dyn PlatformDriver>> {
        let driver_map = &self.0.lock().drivers;
        return driver_map.get(id_table).cloned();
    }

    /// @brief: 注册platform类型驱动
    /// @parameter driver: platform类型驱动，该驱动需要实现PlatformDriver trait
    /// @return: 注册成功，返回Ok(()),，注册失败，返回BusError类型
    #[allow(dead_code)]
    fn register_platform_driver(&self, driver: Arc<dyn PlatformDriver>) -> Result<(), DeviceError> {
        let id_table = driver.id_table();

        let drivers = &mut self.0.lock().drivers;
        // 如果存在同类型的驱动，返回错误
        if drivers.contains_key(&id_table) {
            return Err(DeviceError::DriverExists);
        } else {
            drivers.insert(id_table.clone(), driver.clone());
            return Ok(());
        }
    }

    /// @brief: 卸载platform类型驱动
    /// @parameter driver: platform类型驱动，该驱动需挂载在plaform总线之上
    /// @return: None
    #[allow(dead_code)]
    #[inline]
    fn unregister_platform_driver(
        &mut self,
        driver: Arc<dyn PlatformDriver>,
    ) -> Result<(), DeviceError> {
        let id_table = driver.id_table();
        self.0.lock().drivers.remove(&id_table);
        return Ok(());
    }

    /// @brief: 注册platform类型设备
    /// @parameter driver: platform类型设备，该驱动需要实现PlatformDevice trait
    /// @return: 注册成功，返回Ok(()),，注册失败，返回BusError类型
    #[allow(dead_code)]
    fn register_platform_device(
        &mut self,
        device: Arc<dyn PlatformDevice>,
    ) -> Result<(), DeviceError> {
        let id_table = device.id_table();

        let devices = &mut self.0.lock().devices;
        if devices.contains_key(&id_table) {
            return Err(DeviceError::DeviceExists);
        } else {
            devices.insert(id_table.clone(), device.clone());
            return Ok(());
        }
    }

    /// @brief: 卸载platform类型设备
    /// @parameter device: platform类型设备，该驱设备需挂载在plaform总线之上
    /// @return: None
    #[inline]
    #[allow(dead_code)]
    fn unregister_platform_device(&mut self, device: Arc<dyn PlatformDevice>) {
        let id_table = device.id_table();
        self.0.lock().devices.remove(&id_table);
    }
}

/// @brief: platform总线驱动
#[derive(Debug)]
pub struct PlatformBusDriver {
    drivers: BTreeMap<IdTable, Arc<dyn PlatformDriver>>, // 总线上所有驱动
    devices: BTreeMap<IdTable, Arc<dyn PlatformDevice>>, // 总线上所有设备
    sys_info: Option<Arc<dyn IndexNode>>,
}

impl PlatformBusDriver {
    /// @brief: 创建一个platform总线驱动，该驱动用于匹配plaform总线
    /// @parameter: None
    /// @return: platfor总线驱动
    #[inline]
    #[allow(dead_code)]
    pub fn new() -> Self {
        Self {
            drivers: BTreeMap::new(),
            devices: BTreeMap::new(),
            sys_info: None,
        }
    }
}

/// @brief: 为PlatformBusDriver实现Driver trait
impl Driver for LockedPlatformBusDriver {
    #[inline]
    fn id_table(&self) -> IdTable {
        return IdTable::new("PlatformBusDriver".to_string(), DeviceNumber::new(0));
    }

    fn probe(&self, _data: &DevicePrivateData) -> Result<(), DriverError> {
        todo!()
    }

    fn load(
        &self,
        _data: DevicePrivateData,
        _resource: Option<DeviceResource>,
    ) -> Result<Arc<dyn Device>, DriverError> {
        todo!()
    }
}

/// @brief: 为PlatformBusDriver实现BusDriver trait
impl BusDriver for LockedPlatformBusDriver {
    fn is_empty(&self) -> bool {
        if self.0.lock().devices.is_empty() && self.0.lock().drivers.is_empty() {
            return true;
        } else {
            return false;
        }
    }
}

impl KObject for LockedPlatformBusDriver {
    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn inode(&self) -> Option<Arc<KernFSInode>> {
        todo!()
    }

    fn kobj_type(&self) -> Option<&'static dyn KObjType> {
        todo!()
    }

    fn kset(&self) -> Option<Arc<KSet>> {
        todo!()
    }

    fn parent(&self) -> Option<Weak<dyn KObject>> {
        todo!()
    }

    fn set_inode(&self, inode: Option<Arc<KernFSInode>>) {
        todo!()
    }

    fn kobj_state(&self) -> RwLockReadGuard<KObjectState> {
        todo!()
    }

    fn kobj_state_mut(&self) -> RwLockWriteGuard<KObjectState> {
        todo!()
    }

    fn set_kobj_state(&self, _state: KObjectState) {
        todo!()
    }

    fn name(&self) -> alloc::string::String {
        todo!()
    }

    fn set_name(&self, name: alloc::string::String) {
        todo!()
    }

    fn set_kset(&self, kset: Option<Arc<KSet>>) {
        todo!()
    }

    fn set_parent(&self, parent: Option<Weak<dyn KObject>>) {
        todo!()
    }
}

#[derive(Debug)]
pub struct LockedPlatform(SpinLock<Platform>);

impl LockedPlatform {
    /// @brief: 创建一个加锁的platform总线实例
    /// @parameter: None
    /// @return: platform总线实例
    pub fn new(data: DevicePrivateData, parent: Weak<dyn KObject>) -> LockedPlatform {
        LockedPlatform(SpinLock::new(Platform::new(data, parent)))
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
        let state = self.0.lock().state;
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
        let state = &mut self.0.lock().state;
        *state = set_state;
    }

    /// @brief: 获取总线状态
    /// @parameter: None
    /// @return: 总线状态
    #[inline]
    #[allow(dead_code)]
    fn get_state(&self) -> BusState {
        let state = self.0.lock().state;
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
pub struct Platform {
    data: DevicePrivateData,
    state: BusState,           // 总线状态
    parent: Weak<dyn KObject>, // 总线的父对象

    kernfs_inode: Option<Arc<KernFSInode>>,
}

/// @brief: platform方法集
impl Platform {
    /// @brief: 创建一个platform总线实例
    /// @parameter: None
    /// @return: platform总线实例
    pub fn new(data: DevicePrivateData, parent: Weak<dyn KObject>) -> Self {
        Self {
            data,
            state: BusState::NotInitialized,
            parent,
            kernfs_inode: None,
        }
    }
}

impl KObject for LockedPlatform {
    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn parent(&self) -> Option<Weak<dyn KObject>> {
        Some(self.0.lock().parent.clone())
    }

    fn inode(&self) -> Option<Arc<KernFSInode>> {
        self.0.lock().kernfs_inode.clone()
    }

    fn set_inode(&self, inode: Option<Arc<KernFSInode>>) {
        self.0.lock().kernfs_inode = inode;
    }

    fn kobj_type(&self) -> Option<&'static dyn KObjType> {
        None
    }

    fn kset(&self) -> Option<Arc<KSet>> {
        None
    }

    fn kobj_state(&self) -> RwLockReadGuard<super::kobject::KObjectState> {
        todo!()
    }

    fn kobj_state_mut(&self) -> RwLockWriteGuard<super::kobject::KObjectState> {
        todo!()
    }

    fn set_kobj_state(&self, _state: super::kobject::KObjectState) {
        todo!()
    }

    fn name(&self) -> alloc::string::String {
        todo!()
    }

    fn set_name(&self, name: alloc::string::String) {
        todo!()
    }

    fn set_kset(&self, kset: Option<Arc<KSet>>) {
        todo!()
    }

    fn set_parent(&self, parent: Option<Weak<dyn KObject>>) {
        todo!()
    }
}

/// @brief: 为Platform实现Device trait，platform总线也是一种设备，属于总线设备类型
impl Device for LockedPlatform {
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
}

/// @brief: 为Platform实现Bus trait，platform总线是一种总线设备
impl Bus for LockedPlatform {}

/// @brief: 初始化platform总线
/// @parameter: None
/// @return: None
pub fn platform_bus_init() -> Result<(), SystemError> {
    let platform_driver: Arc<LockedPlatformBusDriver> = Arc::new(LockedPlatformBusDriver::new());
    todo!();
    // let platform_device: Arc<LockedPlatform> =
    //     Arc::new(LockedPlatform::new(DevicePrivateData::new(
    //         IdTable::new("platform".to_string(), DeviceNumber::new(0)),
    //         None,
    //         CompatibleTable::new(vec!["platform"]),
    //         BusState::NotInitialized.into(),
    //     )));
    // bus_register(platform_device.clone()).map_err(|e| e.into())?;
    // platform_device.set_state(BusState::Initialized);
    // //platform_device.set_driver(Some(platform_driver.clone()));
    // bus_driver_register(platform_driver.clone()).map_err(|e| e.into())?;

    return Ok(());
}
