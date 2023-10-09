use alloc::{
    collections::BTreeMap,
    string::ToString,
    sync::{Arc, Weak},
};

use crate::{
    driver::{
        base::{
            device::{
                bus::BusDriver, Device, DeviceError, DeviceNumber, DevicePrivateData,
                DeviceResource, IdTable,
            },
            kobject::{KObjType, KObject, KObjectState},
            kset::KSet,
        },
        Driver,
    },
    filesystem::{kernfs::KernFSInode, vfs::IndexNode},
    libs::{
        rwlock::{RwLockReadGuard, RwLockWriteGuard},
        spinlock::SpinLock,
    },
};

use super::{super::device::driver::DriverError, platform_device::PlatformDevice, CompatibleTable};

lazy_static! {
    static ref PLATFORM_COMPAT_TABLE: CompatibleTable = CompatibleTable::new(vec!["platform"]);
}
/// @brief: 实现该trait的设备驱动实例应挂载在platform总线上，
///         同时应该实现Driver trait
pub trait PlatformDriver: Driver {
    fn compatible_table(&self) -> CompatibleTable;
    /// @brief 探测设备
    /// @param data 设备初始拥有的基本信息
    fn probe(&self, data: DevicePrivateData) -> Result<(), DriverError> {
        if data.compatible_table().matches(&PLATFORM_COMPAT_TABLE) {
            return Ok(());
        } else {
            return Err(DriverError::UnsupportedOperation);
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
