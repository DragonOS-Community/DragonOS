use alloc::{
    collections::BTreeMap,
    string::{String, ToString},
    sync::Arc,
};

use crate::{
    filesystem::{
        sysfs::{
            devices::{sys_device_register, sys_device_unregister},
            SYS_DEVICES_INODE,
        },
        vfs::IndexNode,
    },
    libs::spinlock::SpinLock,
    syscall::SystemError,
};
use core::{any::Any, fmt::Debug};

pub mod bus;
pub mod driver;

lazy_static! {
    pub static ref DEVICE_MANAGER: Arc<LockedDeviceManager> = Arc::new(LockedDeviceManager::new());
}

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

    /// @brief: 将设备标识符转换成name
    /// @parameter None
    /// @return: 设备名
    pub fn to_name(&self) -> String {
        return self.0.to_string() + &':'.to_string() + &self.1.to_string();
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

impl Into<SystemError> for DeviceError {
    fn into(self) -> SystemError {
        match self {
            DeviceError::DriverExists => SystemError::EEXIST,
            DeviceError::DeviceExists => SystemError::EEXIST,
            DeviceError::InitializeFailed => SystemError::EIO,
            DeviceError::NoDeviceForDriver => SystemError::ENODEV,
            DeviceError::NoDriverForDevice => SystemError::ENODEV,
            DeviceError::RegisterError => SystemError::EIO,
        }
    }
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

    /// @brief: 设置sysfs info
    /// @parameter: None
    /// @return: 该设备唯一标识
    fn set_sys_info(&self, sys_info: Option<Arc<dyn IndexNode>>);

    /// @brief: 获取设备的sys information
    /// @parameter id_table: 设备标识符，用于唯一标识该设备
    /// @return: 设备实例
    fn sys_info(&self) -> Option<Arc<dyn IndexNode>>;
}

/// @brief Device管理器(锁)
#[derive(Debug)]
pub struct LockedDeviceManager(SpinLock<DeviceManager>);

impl LockedDeviceManager {
    fn new() -> LockedDeviceManager {
        LockedDeviceManager(SpinLock::new(DeviceManager::new()))
    }

    /// @brief: 添加设备
    /// @parameter id_table: 总线标识符，用于唯一标识该总线
    /// @parameter dev: 设备实例
    /// @return: None
    #[inline]
    #[allow(dead_code)]
    pub fn add_device(&self, id_table: IdTable, dev: Arc<dyn Device>) {
        let mut device_manager = self.0.lock();
        device_manager.devices.insert(id_table, dev);
    }

    /// @brief: 卸载设备
    /// @parameter id_table: 总线标识符，用于唯一标识该设备
    /// @return: None
    #[inline]
    #[allow(dead_code)]
    pub fn remove_device(&self, id_table: &IdTable) {
        let mut device_manager = self.0.lock();
        device_manager.devices.remove(id_table);
    }

    /// @brief: 获取设备
    /// @parameter id_table: 设备标识符，用于唯一标识该设备
    /// @return: 设备实例
    #[inline]
    #[allow(dead_code)]
    pub fn get_device(&self, id_table: &IdTable) -> Option<Arc<dyn Device>> {
        let device_manager = self.0.lock();
        device_manager.devices.get(id_table).cloned()
    }

    /// @brief: 获取设备管理器的sys information
    /// @parameter id_table: 设备标识符，用于唯一标识该设备
    /// @return: 设备实例
    #[inline]
    #[allow(dead_code)]
    fn sys_info(&self) -> Option<Arc<dyn IndexNode>> {
        return self.0.lock().sys_info.clone();
    }
}

/// @brief Device管理器
#[derive(Debug, Clone)]
pub struct DeviceManager {
    devices: BTreeMap<IdTable, Arc<dyn Device>>, // 所有设备
    sys_info: Option<Arc<dyn IndexNode>>,        // sys information
}

impl DeviceManager {
    /// @brief: 创建一个新的设备管理器
    /// @parameter: None
    /// @return: DeviceManager实体
    #[inline]
    fn new() -> DeviceManager {
        DeviceManager {
            devices: BTreeMap::new(),
            sys_info: Some(SYS_DEVICES_INODE()),
        }
    }
}

/// @brief: 设备注册
/// @parameter: name: 设备名
/// @return: 操作成功，返回()，操作失败，返回错误码
pub fn device_register<T: Device>(device: Arc<T>) -> Result<(), DeviceError> {
    DEVICE_MANAGER.add_device(device.get_id_table(), device.clone());
    match sys_device_register(&device.get_id_table().to_name()) {
        Ok(sys_info) => {
            device.set_sys_info(Some(sys_info));
            return Ok(());
        }
        Err(_) => Err(DeviceError::RegisterError),
    }
}

/// @brief: 设备卸载
/// @parameter: name: 设备名
/// @return: 操作成功，返回()，操作失败，返回错误码
pub fn device_unregister<T: Device>(device: Arc<T>) -> Result<(), DeviceError> {
    DEVICE_MANAGER.add_device(device.get_id_table(), device.clone());
    match sys_device_unregister(&device.get_id_table().to_name()) {
        Ok(_) => {
            device.set_sys_info(None);
            return Ok(());
        }
        Err(_) => Err(DeviceError::RegisterError),
    }
}
