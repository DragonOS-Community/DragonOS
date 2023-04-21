use core::{any::Any, fmt::Debug};
use crate::{
    filesystem::{
        sysfs::devices::device_register,
    },
};

pub mod bus;
pub mod driver;

/// @brief: 设备类型
#[allow(dead_code)]
#[derive(Debug, Eq, PartialEq)]
pub enum DeviceType {
    Bus,
    Net,
    Gpu,
    Input,
    Block,
    Rtc,
    Serial,
    Intc,
    PlatformDev,
}

/// @brief: 设备标识符类型
#[derive(Debug, Clone, Hash, PartialOrd, PartialEq, Ord, Eq)]
pub struct IdTable(&'static str, u32);

/// @brief: 设备标识符操作方法集
impl IdTable {
    /// @brief: 创建一个新的设备标识符
    /// @parameter name: 设备名
    /// @parameter id: 设备id
    /// @return: 设备标识符
    pub fn new(name: &'static str, id: u32) -> IdTable {
        Self(name, id)
    }
}

/// @brief: 设备当前状态
#[derive(Debug, Clone, Copy)]
pub enum DeviceState {
    NotInitialized = 0,
    Initialized = 1,
    UnDefined = 2,
}

/// @brief: 设备错误类型
#[derive(Debug, Copy, Clone)]
pub enum DeviceError {
    DriverExists,      // 设备已存在
    DeviceExists,      // 驱动已存在
    InitializeFailed,  // 初始化错误
    NoDeviceForDriver, // 没有合适的设备匹配驱动
    NoDriverForDevice, // 没有合适的驱动匹配设备
    RegisterError,     // 注册失败
}

/// @brief: 将u32类型转换为设备状态类型
impl From<u32> for DeviceState {
    fn from(state: u32) -> Self {
        match state {
            0 => DeviceState::NotInitialized,
            1 => DeviceState::Initialized,
            _ => todo!(),
        }
    }
}

/// @brief: 将设备状态转换为u32类型
impl From<DeviceState> for u32 {
    fn from(state: DeviceState) -> Self {
        match state {
            DeviceState::NotInitialized => 0,
            DeviceState::Initialized => 1,
            DeviceState::UnDefined => 2,
        }
    }
}

/// @brief: 所有设备都应该实现该trait
pub trait Device: Any + Send + Sync + Debug {
    /// @brief: 获取设备类型
    /// @parameter: None
    /// @return: 实现该trait的设备所属类型
    fn get_type(&self) -> DeviceType;

    /// @brief: 获取设备标识
    /// @parameter: None
    /// @return: 该设备唯一标识
    fn get_id_table(&self) -> IdTable;

    /// @brief: 设备注册
    /// @parameter: name: 设备名
    /// @return: 操作成功，返回()，操作失败，返回错误码
    fn register_device(&self, name: &str) -> Result<(), DeviceError> {
        match device_register(name) {
            Ok(_) => Ok(()),
            Err(_) => Err(DeviceError::RegisterError),
        }
    }
}
