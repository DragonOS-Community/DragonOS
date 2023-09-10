use alloc::{collections::BTreeMap, string::String, sync::Arc};

use crate::{
    driver::base::map::{LockedDevsMap, LockedKObjMap},
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

use super::platform::CompatibleTable;

pub mod bus;
pub mod driver;
pub mod init;

lazy_static! {
    pub static ref DEVICE_MANAGER: Arc<LockedDeviceManager> = Arc::new(LockedDeviceManager::new());
}
lazy_static! {
    // 全局字符设备号管理实例
    pub static ref CHARDEVS: Arc<LockedDevsMap> = Arc::new(LockedDevsMap::default());

    // 全局块设备管理实例
    pub static ref BLOCKDEVS: Arc<LockedDevsMap> = Arc::new(LockedDevsMap::default());

    // 全局设备管理实例
    pub static ref DEVMAP: Arc<LockedKObjMap> = Arc::new(LockedKObjMap::default());

}

pub trait KObject: Any + Send + Sync + Debug {}
/// @brief 设备应该实现的操作
/// @usage Device::read_at()
pub trait Device: KObject {
    // TODO: 待实现 open, close
    fn as_any_ref(&self) -> &dyn core::any::Any {
        unimplemented!();
    }
    /// @brief: 获取设备类型
    /// @parameter: None
    /// @return: 实现该trait的设备所属类型
    fn dev_type(&self) -> DeviceType;

    /// @brief: 获取设备标识
    /// @parameter: None
    /// @return: 该设备唯一标识
    fn id_table(&self) -> IdTable {
        unimplemented!();
    }

    /// @brief: 设置sysfs info
    /// @parameter: None
    /// @return: 该设备唯一标识
    fn set_sys_info(&self, sys_info: Option<Arc<dyn IndexNode>>) {
        unimplemented!();
    }

    /// @brief: 获取设备的sys information
    /// @parameter id_table: 设备标识符，用于唯一标识该设备
    /// @return: 设备实例
    fn sys_info(&self) -> Option<Arc<dyn IndexNode>> {
        unimplemented!();
    }
}

// 暂定是不可修改的，在初始化的时候就要确定。以后可能会包括例如硬件中断包含的信息
#[derive(Debug, Clone)]
pub struct DevicePrivateData {
    id_table: IdTable,
    resource: Option<DeviceResource>,
    compatible_table: CompatibleTable,
    state: DeviceState,
}

impl DevicePrivateData {
    pub fn new(
        id_table: IdTable,
        resource: Option<DeviceResource>,
        compatible_table: CompatibleTable,
        state: DeviceState,
    ) -> Self {
        Self {
            id_table,
            resource,
            compatible_table,
            state,
        }
    }

    pub fn id_table(&self) -> &IdTable {
        &self.id_table
    }

    pub fn state(&self) -> DeviceState {
        self.state
    }

    pub fn resource(&self) -> Option<&DeviceResource> {
        self.resource.as_ref()
    }

    pub fn compatible_table(&self) -> &CompatibleTable {
        &self.compatible_table
    }

    pub fn set_state(&mut self, state: DeviceState) {
        self.state = state;
    }
}

#[derive(Debug, Clone)]
pub struct DeviceResource {
    //可能会用来保存例如 IRQ PWM 内存地址等需要申请的资源，将来由资源管理器+Framework框架进行管理。
}

impl Default for DeviceResource {
    fn default() -> Self {
        return Self {};
    }
}

/// @brief: 设备号实例
#[derive(Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct DeviceNumber(usize);

impl Default for DeviceNumber {
    fn default() -> Self {
        DeviceNumber(0)
    }
}

impl From<usize> for DeviceNumber {
    fn from(dev_t: usize) -> Self {
        DeviceNumber(dev_t)
    }
}

impl Into<usize> for DeviceNumber {
    fn into(self) -> usize {
        self.0
    }
}

impl DeviceNumber {
    /// @brief: 设备号创建
    /// @parameter: dev_t: 设备号
    /// @return: 设备号实例
    pub fn new(dev_t: usize) -> DeviceNumber {
        Self(dev_t)
    }

    /// @brief: 获取主设备号
    /// @parameter: none
    /// @return: 主设备号
    pub fn major(&self) -> usize {
        (self.0 >> 20) & 0xfff
    }

    /// @brief: 获取次设备号
    /// @parameter: none
    /// @return: 次设备号
    pub fn minor(&self) -> usize {
        self.0 & 0xfffff
    }

    pub fn from_major_minor(major: usize, minor: usize) -> usize {
        ((major & 0xffffff) << 8) | (minor & 0xff)
    }
}

/// @brief: 根据主次设备号创建设备号实例
/// @parameter: major: 主设备号
///             minor: 次设备号
/// @return: 设备号实例
pub fn mkdev(major: usize, minor: usize) -> DeviceNumber {
    DeviceNumber(((major & 0xfff) << 20) | (minor & 0xfffff))
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
pub struct IdTable(&'static str, DeviceNumber);

/// @brief: 设备标识符操作方法集
impl IdTable {
    /// @brief: 创建一个新的设备标识符
    /// @parameter name: 设备名
    /// @parameter id: 设备id
    /// @return: 设备标识符
    pub fn new(name: &'static str, id: DeviceNumber) -> IdTable {
        Self(name, id)
    }

    /// @brief: 将设备标识符转换成name
    /// @parameter None
    /// @return: 设备名
    pub fn to_name(&self) -> String {
        return format!("{}:{:?}", self.0, self.1 .0);
    }

    pub fn name(&self) -> String {
        return self.name().clone();
    }

    pub fn device_number(&self) -> DeviceNumber {
        return self.1;
    }
}

impl Default for IdTable {
    fn default() -> Self {
        IdTable("unknown", DeviceNumber::new(0))
    }
}

// 以现在的模型，设备在加载到系统中就是已经初始化的状态了，因此可以考虑把这个删掉
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
    DriverExists,         // 设备已存在
    DeviceExists,         // 驱动已存在
    InitializeFailed,     // 初始化错误
    UnInitializedDevice,  // 未初始化的设备
    NoDeviceForDriver,    // 没有合适的设备匹配驱动
    NoDriverForDevice,    // 没有合适的驱动匹配设备
    RegisterError,        // 注册失败
    UnsupportedOperation, // 不支持的操作
}

impl Into<SystemError> for DeviceError {
    fn into(self) -> SystemError {
        match self {
            DeviceError::DriverExists => SystemError::EEXIST,
            DeviceError::DeviceExists => SystemError::EEXIST,
            DeviceError::InitializeFailed => SystemError::EIO,
            DeviceError::UnInitializedDevice => SystemError::ENODEV,
            DeviceError::NoDeviceForDriver => SystemError::ENODEV,
            DeviceError::NoDriverForDevice => SystemError::ENODEV,
            DeviceError::RegisterError => SystemError::EIO,
            DeviceError::UnsupportedOperation => SystemError::EIO,
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
    DEVICE_MANAGER.add_device(device.id_table(), device.clone());
    match sys_device_register(&device.id_table().to_name()) {
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
    DEVICE_MANAGER.add_device(device.id_table(), device.clone());
    match sys_device_unregister(&device.id_table().to_name()) {
        Ok(_) => {
            device.set_sys_info(None);
            return Ok(());
        }
        Err(_) => Err(DeviceError::RegisterError),
    }
}
