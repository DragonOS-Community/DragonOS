use super::{driver::Driver, Device, DeviceState, IdTable};
use crate::libs::spinlock::SpinLock;
use alloc::{collections::BTreeMap, sync::Arc};
use core::fmt::Debug;
use lazy_static::lazy_static;

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
}

/// @brief: 总线管理结构体加锁
pub struct BusManagerLock(SpinLock<BusManager>);

/// @brief: 总线管理方法集
impl BusManagerLock {
    /// @brief: 创建总线管理实例
    /// @parameter: None
    /// @return: 总线管理实例
    #[inline]
    #[allow(dead_code)]
    pub fn new() -> Self {
        BusManagerLock(SpinLock::new(BusManager {
            buses: BTreeMap::new(),
            bus_drvs: BTreeMap::new(),
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
    pub fn add_bus_driver(&self, id_table: IdTable, bus_drv: Arc<dyn BusDriver>) {
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
    pub fn get_bus_driver(&self, id_table: &IdTable) -> Option<Arc<dyn BusDriver>> {
        let bus_manager = self.0.lock();
        return bus_manager.bus_drvs.get(id_table).cloned();
    }
}

lazy_static! {
    pub static ref BUS_MANAGER: Arc<BusManagerLock> = Arc::new(BusManagerLock::new());
}
