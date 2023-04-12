use super::IdTable;
use core::{any::Any, fmt::Debug};

/// @brief: Driver error
#[allow(dead_code)]
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum DriverError {
    ProbeError,
}

/// @brief: 所有设备驱动都应该实现该trait
pub trait Driver: Any + Send + Sync + Debug {
    /// @brief: 获取设备驱动标识符
    /// @parameter: None
    /// @return: 该设备驱动唯一标识符
    fn get_id_table(&self) -> IdTable;
}
