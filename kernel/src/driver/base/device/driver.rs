use super::IdTable;
use crate::{
    driver::Driver, filesystem::vfs::IndexNode, libs::spinlock::SpinLock, syscall::SystemError,
};
use alloc::{collections::BTreeMap, sync::Arc};
use core::fmt::Debug;

lazy_static! {
    pub static ref DRIVER_MANAGER: Arc<LockedDriverManager> = Arc::new(LockedDriverManager::new());
}

/// @brief: Driver error
#[allow(dead_code)]
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum DriverError {
    ProbeError,            // 探测设备失败(该驱动不能初始化这个设备)
    RegisterError,         // 设备注册失败
    AllocateResourceError, // 获取设备所需资源失败
    UnsupportedOperation,  // 不支持的操作
    UnInitialized,         // 未初始化
}

impl Into<SystemError> for DriverError {
    fn into(self) -> SystemError {
        match self {
            DriverError::ProbeError => SystemError::ENODEV,
            DriverError::RegisterError => SystemError::ENODEV,
            DriverError::AllocateResourceError => SystemError::EIO,
            DriverError::UnsupportedOperation => SystemError::EIO,
            DriverError::UnInitialized => SystemError::ENODEV,
        }
    }
}

/// @brief: 驱动管理器(锁)
#[derive(Debug)]
pub struct LockedDriverManager(SpinLock<DriverManager>);

impl LockedDriverManager {
    /// @brief: 创建一个新的驱动管理器(锁)
    /// @parameter None
    /// @return: LockedDriverManager实体
    #[inline]
    fn new() -> LockedDriverManager {
        LockedDriverManager(SpinLock::new(DriverManager::new()))
    }

    /// @brief: 添加驱动
    /// @parameter id_table: 驱动标识符，用于唯一标识该驱动
    /// @parameter drv: 驱动实例
    /// @return: None
    #[inline]
    #[allow(dead_code)]
    pub fn add_driver(&self, id_table: IdTable, drv: Arc<dyn Driver>) {
        let mut driver_manager = self.0.lock();
        driver_manager.drivers.insert(id_table, drv);
    }

    /// @brief: 卸载驱动
    /// @parameter id_table: 驱动标识符，用于唯一标识该驱动
    /// @return: None
    #[inline]
    #[allow(dead_code)]
    pub fn remove_driver(&self, id_table: &IdTable) {
        let mut driver_manager = self.0.lock();
        driver_manager.drivers.remove(id_table);
    }

    /// @brief: 获取驱动
    /// @parameter id_table: 驱动标识符，用于唯一标识该驱动
    /// @return: 驱动实例
    #[inline]
    #[allow(dead_code)]
    pub fn get_driver(&self, id_table: &IdTable) -> Option<Arc<dyn Driver>> {
        let driver_manager = self.0.lock();
        driver_manager.drivers.get(id_table).cloned()
    }

    /// @brief: 获取驱动管理器的sys information
    /// @parameter id_table: 设备标识符，用于唯一标识该驱动
    /// @return: 驱动实例
    #[inline]
    #[allow(dead_code)]
    fn get_sys_info(&self) -> Option<Arc<dyn IndexNode>> {
        return self.0.lock().sys_info.clone();
    }
}

/// @brief: 驱动管理器
#[derive(Debug, Clone)]
pub struct DriverManager {
    drivers: BTreeMap<IdTable, Arc<dyn Driver>>, // 所有驱动
    sys_info: Option<Arc<dyn IndexNode>>,        // sys information
}

impl DriverManager {
    /// @brief: 创建一个新的设备管理器
    /// @parameter: None
    /// @return: Manager实体
    #[inline]
    fn new() -> DriverManager {
        DriverManager {
            drivers: BTreeMap::new(),
            sys_info: None,
        }
    }
}

/// @brief: 驱动注册
/// @parameter: name: 驱动名
/// @return: 操作成功，返回()，操作失败，返回错误码
pub fn driver_register(driver: Arc<dyn Driver>) -> Result<(), DriverError> {
    DRIVER_MANAGER.add_driver(driver.id_table(), driver);
    return Ok(());
}

/// @brief: 驱动卸载
/// @parameter: name: 驱动名
/// @return: 操作成功，返回()，操作失败，返回错误码
#[allow(dead_code)]
pub fn driver_unregister(driver: Arc<dyn Driver>) -> Result<(), DriverError> {
    DRIVER_MANAGER.remove_driver(&driver.id_table());
    return Ok(());
}
