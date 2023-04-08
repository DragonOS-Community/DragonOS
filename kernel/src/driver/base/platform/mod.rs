use super::device::{
    bus::{Bus, BusDriver, BusState, BUS_MANAGER},
    driver::Driver,
    Device, DeviceError, DeviceState, DeviceType, IdTable,
};
use crate::libs::{mutex::Mutex, rwlock::RwLock};
use alloc::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    vec::Vec,
};
use core::fmt::Debug;
use lazy_static::lazy_static;
use platform_device::PlatformDevice;
use platform_driver::PlatformDriver;

pub mod platform_device;
pub mod platform_driver;

/// @brief: platform总线匹配表
///         总线上的设备和驱动都存在一份匹配表
///         根据匹配表条目是否匹配来辨识设备和驱动能否进行匹配
#[derive(Debug)]
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
        for id in &self.0 {
            if other.0.contains(id) {
                return true;
            }
        }
        return false;
    }
}

/// @brief: platform总线驱动
#[derive(Debug)]
pub struct PlatformBusDriver {
    drivers: RwLock<BTreeMap<IdTable, Arc<dyn PlatformDriver>>>, // 总线上所有驱动
    devices: RwLock<BTreeMap<IdTable, Arc<dyn PlatformDevice>>>, // 总线上所有设备
}

impl PlatformBusDriver {
    /// @brief: 创建一个platform总线驱动，该驱动用于匹配plaform总线
    /// @parameter: None
    /// @return: platfor总线驱动
    #[inline]
    #[allow(dead_code)]
    pub fn new() -> Self {
        Self {
            drivers: RwLock::new(BTreeMap::new()),
            devices: RwLock::new(BTreeMap::new()),
        }
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
        let device_map = self.devices.read();
        return device_map.get(id_table).cloned();
    }

    /// @brief: 根据设备驱动标识符获取platform总线上的驱动
    /// @parameter id_table: 设备驱动标识符
    /// @return: 总线上的驱动
    #[inline]
    #[allow(dead_code)]
    fn get_driver(&self, id_table: &IdTable) -> Option<Arc<dyn PlatformDriver>> {
        let driver_map = self.drivers.read();
        return driver_map.get(id_table).cloned();
    }

    /// @brief: 注册platform类型驱动
    /// @parameter driver: platform类型驱动，该驱动需要实现PlatformDriver trait
    /// @return: 注册成功，返回Ok(()),，注册失败，返回BusError类型
    #[allow(dead_code)]
    fn register_platform_driver(
        &mut self,
        driver: Arc<dyn PlatformDriver>,
    ) -> Result<(), DeviceError> {
        let id_table = driver.get_id_table();

        let mut drivers = self.drivers.write();
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
    fn unregister_platform_driver(&mut self, driver: Arc<dyn PlatformDriver>) {
        let id_table = driver.get_id_table();
        self.drivers.write().remove(&id_table);
    }

    /// @brief: 注册platform类型设备
    /// @parameter driver: platform类型设备，该驱动需要实现PlatformDevice trait
    /// @return: 注册成功，返回Ok(()),，注册失败，返回BusError类型
    #[allow(dead_code)]
    fn register_platform_device(
        &mut self,
        device: Arc<dyn PlatformDevice>,
    ) -> Result<(), DeviceError> {
        let id_table = device.get_id_table();

        let mut devices = self.devices.write();
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
        let id_table = device.get_id_table();
        self.devices.write().remove(&id_table);
    }

    /// @brief: 匹配platform类型驱动
    /// @parameter driver: platform类型驱动
    /// @return: 如果匹配成功，返回成功驱动的设备数，否则，返回BusError类型
    #[allow(dead_code)]
    fn driver_match_device(&self, driver: Arc<dyn PlatformDriver>) -> Result<i32, DeviceError> {
        let mut num = 0;
        let devices = self.devices.read();

        for (_dev_id_table, device) in devices.iter() {
            if device
                .get_compatible_table()
                .matches(&driver.get_compatible_table())
            {
                if !device.is_initialized() {
                    // 设备未初始化，调用驱动probe函数
                    match driver.probe(device.clone()) {
                        Ok(()) => {
                            num = num + 1;
                            device.set_state(DeviceState::Initialized)
                        },
                        // 可以驱动很多设备，一个设备初始化出错即返回
                        Err(_) => return Err(DeviceError::InitializeFailed),
                    }
                }
            }
        }
        if num == 0 {
            return Err(DeviceError::NoDeviceForDriver);
        } else {
            return Ok(num);
        }
    }

    /// @brief: 匹配platform上的设备
    /// @parameter driver: platform类型设备
    /// @return: 如果匹配成功，返回Ok(())，否则，返回BusError类型
    #[allow(dead_code)]
    fn device_match_driver(&self, device: Arc<dyn PlatformDevice>) -> Result<(), DeviceError> {
        let drivers = self.drivers.read();
        for (_drv_id_table, driver) in &*drivers {
            if driver
                .get_compatible_table()
                .matches(&device.get_compatible_table())
            {
                match driver.probe(device.clone()) {
                    Ok(_driver) => {
                        // 将设备状态置为已初始化
                        device.set_state(DeviceState::Initialized);
                        return Ok(());
                    }
                    Err(_) => return Err(DeviceError::InitializeFailed),
                }
            }
        }
        return Err(DeviceError::NoDriverForDevice);
    }
}

/// @brief: 为PlatformBusDriver实现Driver trait
impl Driver for PlatformBusDriver {
    fn get_id_table(&self) -> IdTable {
        IdTable::new("PlatformBusDriver", 0)
    }
}

/// @brief: 为PlatformBusDriver实现BusDriver trait
impl BusDriver for PlatformBusDriver {
    fn is_empty(&self) -> bool {
        if self.devices.read().is_empty() && self.drivers.read().is_empty() {
            return true;
        } else {
            return false;
        }
    }
}

/// @brief: platform总线
#[derive(Debug, Clone)]
pub struct Platform {
    state: Arc<Mutex<BusState>>,            // 总线状态
    driver: Option<Arc<PlatformBusDriver>>, // 总线驱动
}

/// @brief: platform方法集
impl Platform {
    /// @brief: 创建一个platform总线实例
    /// @parameter: None
    /// @return: platform总线实例
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(BusState::NotInitialized)),
            driver: Option::None,
        }
    }

    /// @brief: 获取总线的匹配表
    /// @parameter: None
    /// @return: platform总线匹配表
    #[inline]
    #[allow(dead_code)]
    fn get_compatible_table(&self) -> CompatibleTable {
        CompatibleTable::new(vec!["platform"])
    }

    /// @brief: 判断总线是否初始化
    /// @parameter: None
    /// @return: 已初始化，返回true，否则，返回false
    #[inline]
    #[allow(dead_code)]
    fn is_initialized(&self) -> bool {
        let state = self.state.lock();
        match *state {
            BusState::Initialized => true,
            _ => false,
        }
    }

    /// @brief: 设置总线状态
    /// @parameter set_state: 总线状态BusState
    /// @return: None
    #[inline]
    fn set_state(&self, set_state: BusState) {
        let mut state = self.state.lock();
        *state = set_state;
    }

    /// @brief: 获取总线状态
    /// @parameter: None
    /// @return: 总线状态
    #[inline]
    #[allow(dead_code)]
    fn get_state(&self) -> BusState {
        let state = self.state.lock();
        return *state;
    }

    /// @brief:
    /// @parameter: None
    /// @return: 总线状态
    #[inline]
    #[allow(dead_code)]
    fn set_driver(&mut self, driver: Option<Arc<PlatformBusDriver>>) {
        self.driver = driver;
    }
}

/// @brief: 为Platform实现Device trait，platform总线也是一种设备，属于总线设备类型
impl Device for Platform {
    /// @brief: 获取platform设备类型
    /// @parameter: None
    /// @return: Bus类型
    #[inline]
    #[allow(dead_code)]
    fn get_type(&self) -> DeviceType {
        return DeviceType::Bus;
    }

    /// @brief: 获取platform设备标识符
    /// @parameter: None
    /// @return: platform总线设备标识符
    #[inline]
    #[allow(dead_code)]
    fn get_id_table(&self) -> IdTable {
        IdTable::new("platform", 0)
    }
}

/// @brief: 为Platform实现Bus trait，platform总线是一种总线设备
impl Bus for Platform {}

lazy_static! {
    pub static ref BUS_PLATFORM_DRIVER: Arc<PlatformBusDriver> = Arc::new(PlatformBusDriver::new());
    pub static ref BUS_PLATFORM_DEVICE: Arc<Platform> = Arc::new(Platform::new());
}

/// @brief: 初始化platform总线
/// @parameter: None
/// @return: None
#[allow(dead_code)]
pub fn platform_bus_init() {
    BUS_MANAGER.add_bus_driver(
        BUS_PLATFORM_DRIVER.get_id_table(),
        BUS_PLATFORM_DRIVER.clone(),
    );
    BUS_MANAGER.add_bus(
        BUS_PLATFORM_DEVICE.get_id_table(),
        BUS_PLATFORM_DEVICE.clone(),
    );
    BUS_PLATFORM_DEVICE.set_state(BusState::Initialized);
}

#[no_mangle]
extern "C" fn c_platform_bus_init() {
    BUS_MANAGER.add_bus_driver(
        BUS_PLATFORM_DRIVER.get_id_table(),
        BUS_PLATFORM_DRIVER.clone(),
    );
    BUS_MANAGER.add_bus(
        BUS_PLATFORM_DEVICE.get_id_table(),
        BUS_PLATFORM_DEVICE.clone(),
    );
    BUS_PLATFORM_DEVICE.set_state(BusState::Initialized);
}
