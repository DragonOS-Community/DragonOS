use self::{platform_device::PlatformBusDevice, subsys::PlatformBus};

use super::{
    device::{
        bus::{bus_driver_register, bus_register, Bus, BusDriver, BusState},
        device_unregister,
        driver::DriverError,
        sys_devices_kset, Device, DeviceError, DeviceNumber, DevicePrivateData, DeviceResource,
        DeviceType, IdTable,
    },
    kobject::KObject,
    kset::KSet,
};
use crate::{
    driver::{
        base::{device::device_register, platform::platform_driver::LockedPlatformBusDriver},
        Driver,
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

pub mod platform_device;
pub mod platform_driver;
pub mod subsys;

static mut PLATFORM_DEVICE: Option<Arc<PlatformBusDevice>> = None;
static mut PLATFORM_BUS: Option<Arc<PlatformBus>> = None;

#[inline(always)]
pub fn platform_device() -> Arc<PlatformBusDevice> {
    unsafe { PLATFORM_DEVICE.clone().unwrap() }
}

#[inline(always)]
pub fn platform_bus() -> Arc<PlatformBus> {
    unsafe { PLATFORM_BUS.clone().unwrap() }
}

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

/// @brief: 初始化platform总线
/// @parameter: None
/// @return: None
/// 
/// 参考： https://opengrok.ringotek.cn/xref/linux-6.1.9/drivers/base/platform.c?fi=platform_bus_init#1511
pub fn platform_bus_init() -> Result<(), SystemError> {
    let platform_driver: Arc<LockedPlatformBusDriver> = Arc::new(LockedPlatformBusDriver::new());

    let platform_device: Arc<PlatformBusDevice> = PlatformBusDevice::new(
        DevicePrivateData::new(
            IdTable::new("platform".to_string(), DeviceNumber::new(0)),
            None,
            CompatibleTable::new(vec!["platform"]),
            BusState::NotInitialized.into(),
        ),
        Arc::downgrade(&(sys_devices_kset() as Arc<dyn KObject>)),
    );
    unsafe { PLATFORM_DEVICE = Some(platform_device.clone()) };
    device_register(platform_device.clone())?;

    let paltform_bus = PlatformBus::new();
    let r = bus_register(paltform_bus.clone() as Arc<dyn Bus>);
    if r.is_err() {
        device_unregister(platform_device.clone());
        unsafe { PLATFORM_DEVICE = None };
        return r;
    }
    unsafe { PLATFORM_BUS = Some(paltform_bus) };

    return r;
}
