use self::{platform_device::PlatformBusDevice, subsys::PlatformBus};

use super::{
    device::{
        bus::{bus_register, Bus, BusState},
        device_unregister, sys_devices_kset, DevicePrivateData, IdTable,
    },
    kobject::KObject,
};
use crate::driver::base::device::device_register;
use alloc::{collections::BTreeSet, string::ToString, sync::Arc, vec::Vec};
use core::fmt::Debug;
use system_error::SystemError;
use unified_init::{define_unified_initializer_slice, unified_init};

pub mod platform_device;
pub mod platform_driver;
pub mod subsys;

static mut PLATFORM_BUS_DEVICE: Option<Arc<PlatformBusDevice>> = None;
static mut PLATFORM_BUS: Option<Arc<PlatformBus>> = None;

define_unified_initializer_slice!(PLATFORM_DEVICE_INITIALIZER);

#[allow(dead_code)]
#[inline(always)]
pub fn platform_bus_device() -> Arc<PlatformBusDevice> {
    unsafe { PLATFORM_BUS_DEVICE.clone().unwrap() }
}

#[allow(dead_code)]
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
/// 参考： https://code.dragonos.org.cn/xref/linux-6.1.9/drivers/base/platform.c?fi=platform_bus_init#1511
pub fn platform_bus_init() -> Result<(), SystemError> {
    let platform_device: Arc<PlatformBusDevice> = PlatformBusDevice::new(
        DevicePrivateData::new(
            IdTable::new("platform".to_string(), None),
            BusState::NotInitialized.into(),
        ),
        Some(Arc::downgrade(&(sys_devices_kset() as Arc<dyn KObject>))),
    );
    unsafe { PLATFORM_BUS_DEVICE = Some(platform_device.clone()) };
    // 注册到/sys/devices下
    device_register(platform_device.clone())?;

    let paltform_bus = PlatformBus::new();
    // 注册到/sys/bus下
    let r = bus_register(paltform_bus.clone() as Arc<dyn Bus>);
    if r.is_err() {
        device_unregister(platform_device.clone());
        unsafe { PLATFORM_BUS_DEVICE = None };
        return r;
    }
    unsafe { PLATFORM_BUS = Some(paltform_bus) };

    unified_init!(PLATFORM_DEVICE_INITIALIZER);

    return r;
}
