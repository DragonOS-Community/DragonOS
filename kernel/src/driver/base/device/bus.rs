use super::{
    device_register, device_unregister,
    driver::{driver_register, driver_unregister, DriverError},
    Device, DeviceError, DeviceState, IdTable,
};
use crate::{
    driver::Driver,
    filesystem::{
        sysfs::{
            bus::{sys_bus_init, sys_bus_register},
            SYS_BUS_INODE,
        },
        vfs::IndexNode,
    },
    libs::spinlock::SpinLock,
};
use alloc::{collections::BTreeMap, sync::Arc};
use core::fmt::Debug;
use lazy_static::lazy_static;

lazy_static! {
    pub static ref BUS_MANAGER: Arc<LockedBusManager> = Arc::new(LockedBusManager::new());
}

/// @brief: 总线状态
#[derive(Debug, Copy, Clone)]
pub enum BusState {
    NotInitialized = 0, // 未初始化
    Initialized = 1,    // 已初始化
    UnDefined = 2,      // 未定义的
}

/// @brief: 将u32类型转换为总线状态类型
impl From<u32> for BusState {
    fn from(state: u32) -> Self {
        match state {
            0 => BusState::NotInitialized,
            1 => BusState::Initialized,
            _ => BusState::UnDefined,
        }
    }
}

/// @brief: 将总线状态类型转换为u32类型
impl From<DeviceState> for BusState {
    fn from(state: DeviceState) -> Self {
        match state {
            DeviceState::Initialized => BusState::Initialized,
            DeviceState::NotInitialized => BusState::NotInitialized,
            DeviceState::UnDefined => BusState::UnDefined,
        }
    }
}

/// @brief: 将总线状态类型转换为设备状态类型
impl From<BusState> for DeviceState {
    fn from(state: BusState) -> Self {
        match state {
            BusState::Initialized => DeviceState::Initialized,
            BusState::NotInitialized => DeviceState::NotInitialized,
            BusState::UnDefined => DeviceState::UnDefined,
        }
    }
}

/// @brief: 总线驱动trait，所有总线驱动都应实现该trait
pub trait BusDriver: Driver {
    /// @brief: 判断总线是否为空
    /// @parameter: None
    /// @return: 如果总线上设备和驱动的数量都为0，则返回true，否则，返回false
    fn is_empty(&self) -> bool;
}

/// @brief: 总线设备trait，所有总线都应实现该trait
pub trait Bus: Device {}

/// @brief: 总线管理结构体
#[derive(Debug, Clone)]
pub struct BusManager {
    buses: BTreeMap<IdTable, Arc<dyn Bus>>,          // 总线设备表
    bus_drvs: BTreeMap<IdTable, Arc<dyn BusDriver>>, // 总线驱动表
    sys_info: Option<Arc<dyn IndexNode>>,            // 总线inode
}

/// @brief: bus管理(锁)
pub struct LockedBusManager(SpinLock<BusManager>);

/// @brief: 总线管理方法集
impl LockedBusManager {
    /// @brief: 创建总线管理实例
    /// @parameter: None
    /// @return: 总线管理实例
    #[inline]
    #[allow(dead_code)]
    pub fn new() -> Self {
        LockedBusManager(SpinLock::new(BusManager {
            buses: BTreeMap::new(),
            bus_drvs: BTreeMap::new(),
            sys_info: Some(SYS_BUS_INODE()),
        }))
    }

    /// @brief: 添加总线
    /// @parameter id_table: 总线标识符，用于唯一标识该总线
    /// @parameter bus_dev: 总线实例
    /// @return: None
    #[inline]
    #[allow(dead_code)]
    pub fn add_bus(&self, id_table: IdTable, bus_dev: Arc<dyn Bus>) {
        let mut bus_manager = self.0.lock();
        bus_manager.buses.insert(id_table, bus_dev);
    }

    /// @brief: 添加总线驱动
    /// @parameter id_table: 总线驱动标识符，用于唯一标识该总线驱动
    /// @parameter bus_dev: 总线驱动实例
    /// @return: None
    #[inline]
    #[allow(dead_code)]
    pub fn add_driver(&self, id_table: IdTable, bus_drv: Arc<dyn BusDriver>) {
        let mut bus_manager = self.0.lock();
        bus_manager.bus_drvs.insert(id_table, bus_drv);
    }

    /// @brief: 卸载总线
    /// @parameter id_table: 总线标识符，用于唯一标识该总线
    /// @return: None
    #[inline]
    #[allow(dead_code)]
    pub fn remove_bus(&self, id_table: &IdTable) {
        let mut bus_manager = self.0.lock();
        bus_manager.buses.remove(id_table);
    }

    /// @brief: 卸载总线驱动
    /// @parameter id_table: 总线驱动标识符，用于唯一标识该总线驱动
    /// @return: None
    #[inline]
    #[allow(dead_code)]
    pub fn remove_bus_driver(&self, id_table: &IdTable) {
        let mut bus_manager = self.0.lock();
        bus_manager.bus_drvs.remove(id_table);
    }

    /// @brief: 获取总线设备
    /// @parameter id_table: 总线标识符，用于唯一标识该总线
    /// @return: 总线设备实例
    #[inline]
    #[allow(dead_code)]
    pub fn get_bus(&self, id_table: &IdTable) -> Option<Arc<dyn Bus>> {
        let bus_manager = self.0.lock();
        bus_manager.buses.get(id_table).cloned()
    }

    /// @brief: 获取总线驱动
    /// @parameter id_table: 总线驱动标识符，用于唯一标识该总线驱动
    /// @return: 总线驱动实例
    #[inline]
    #[allow(dead_code)]
    pub fn get_driver(&self, id_table: &IdTable) -> Option<Arc<dyn BusDriver>> {
        let bus_manager = self.0.lock();
        return bus_manager.bus_drvs.get(id_table).cloned();
    }

    /// @brief: 获取总线管理器的sys information
    /// @parameter None
    /// @return: sys inode
    #[inline]
    #[allow(dead_code)]
    fn sys_info(&self) -> Option<Arc<dyn IndexNode>> {
        return self.0.lock().sys_info.clone();
    }
}

/// @brief: 总线注册，将总线加入全局总线管理器中，并根据id table在sys/bus和sys/devices下生成文件夹
/// @parameter bus: Bus设备实体
/// @return: 成功:()   失败:DeviceError
pub fn bus_register<T: Bus>(bus: Arc<T>) -> Result<(), DeviceError> {
    BUS_MANAGER.add_bus(bus.id_table(), bus.clone());
    match sys_bus_register(&bus.id_table().name()) {
        Ok(inode) => {
            let _ = sys_bus_init(&inode);
            return device_register(bus);
        }
        Err(_) => Err(DeviceError::RegisterError),
    }
}

/// @brief: 总线注销，将总线从全局总线管理器中删除，并在sys/bus和sys/devices下删除文件夹
/// @parameter bus: Bus设备实体
/// @return: 成功:()   失败:DeviceError
#[allow(dead_code)]
pub fn bus_unregister<T: Bus>(bus: Arc<T>) -> Result<(), DeviceError> {
    BUS_MANAGER.add_bus(bus.id_table(), bus.clone());
    return device_unregister(bus);
}

/// @brief: 总线驱动注册，将总线驱动加入全局总线管理器中
/// @parameter bus: Bus设备驱动实体
/// @return: 成功:()   失败:DeviceError
pub fn bus_driver_register(bus_driver: Arc<dyn BusDriver>) -> Result<(), DriverError> {
    BUS_MANAGER.add_driver(bus_driver.id_table(), bus_driver.clone());
    return driver_register(bus_driver);
}

/// @brief: 总线驱动注销，将总线从全局总线管理器中删除
/// @parameter bus: Bus设备驱动实体
/// @return: 成功:()   失败:DeviceError
#[allow(dead_code)]
pub fn bus_driver_unregister(bus_driver: Arc<dyn BusDriver>) -> Result<(), DriverError> {
    BUS_MANAGER.add_driver(bus_driver.id_table(), bus_driver.clone());
    return driver_unregister(bus_driver);
}
