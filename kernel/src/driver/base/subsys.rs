use core::{
    fmt::Debug,
    sync::atomic::{AtomicBool, Ordering},
};

use crate::libs::{
    notifier::AtomicNotifierChain,
    rwlock::{RwLock, RwLockReadGuard},
    spinlock::SpinLock,
};
use alloc::{
    string::String,
    sync::{Arc, Weak},
    vec::Vec,
};
use system_error::SystemError;

use super::{
    class::Class,
    device::{
        bus::{Bus, BusNotifyEvent},
        driver::Driver,
        Device,
    },
    kobject::KObject,
    kset::KSet,
};

/// 一个用于存储bus/class的驱动核心部分的信息的结构体
#[derive(Debug)]
pub struct SubSysPrivate {
    /// 用于定义这个子系统的kset
    subsys: Arc<KSet>,
    ksets: RwLock<SubSysKSets>,
    /// 指向拥有当前结构体的`dyn bus`对象的弱引用
    bus: SpinLock<Option<Weak<dyn Bus>>>,
    /// 指向拥有当前结构体的`dyn class`对象的弱引用
    class: SpinLock<Option<Weak<dyn Class>>>,
    drivers_autoprobe: AtomicBool,
    /// 当前总线上的所有设备
    devices: RwLock<Vec<Arc<dyn Device>>>,
    /// 当前总线上的所有驱动
    drivers: RwLock<Vec<Arc<dyn Driver>>>,
    interfaces: &'static [&'static dyn SubSysInterface],
    bus_notifier: AtomicNotifierChain<BusNotifyEvent, Arc<dyn Device>>,
}

#[derive(Debug)]
struct SubSysKSets {
    /// 子系统的`devices`目录
    devices_kset: Option<Arc<KSet>>,
    /// 子系统的`drivers`目录
    drivers_kset: Option<Arc<KSet>>,
}

impl SubSysKSets {
    pub fn new() -> Self {
        return Self {
            devices_kset: None,
            drivers_kset: None,
        };
    }
}

impl SubSysPrivate {
    pub fn new(
        name: String,
        bus: Option<Weak<dyn Bus>>,
        class: Option<Weak<dyn Class>>,
        interfaces: &'static [&'static dyn SubSysInterface],
    ) -> Self {
        let subsys = KSet::new(name);
        return Self {
            subsys,
            ksets: RwLock::new(SubSysKSets::new()),
            drivers_autoprobe: AtomicBool::new(false),
            bus: SpinLock::new(bus),
            class: SpinLock::new(class),
            devices: RwLock::new(Vec::new()),
            drivers: RwLock::new(Vec::new()),
            interfaces,
            bus_notifier: AtomicNotifierChain::new(),
        };
    }

    pub fn name(&self) -> String {
        return self.subsys.name();
    }

    pub fn subsys(&self) -> Arc<KSet> {
        return self.subsys.clone();
    }

    #[inline]
    #[allow(dead_code)]
    pub fn bus(&self) -> Option<Weak<dyn Bus>> {
        return self.bus.lock().clone();
    }

    pub fn set_bus(&self, bus: Option<Weak<dyn Bus>>) {
        *self.bus.lock() = bus;
    }

    #[allow(dead_code)]
    #[inline]
    pub fn class(&self) -> Option<Weak<dyn Class>> {
        let mut guard = self.class.lock();
        if let Some(r) = guard.clone() {
            return Some(r);
        } else {
            *guard = None;
            return None;
        }
    }

    pub fn set_class(&self, class: Option<Weak<dyn Class>>) {
        *self.class.lock() = class;
    }

    pub fn devices(&self) -> RwLockReadGuard<Vec<Arc<dyn Device>>> {
        return self.devices.read();
    }

    pub fn drivers(&self) -> RwLockReadGuard<Vec<Arc<dyn Driver>>> {
        return self.drivers.read();
    }

    pub fn drivers_autoprobe(&self) -> bool {
        return self.drivers_autoprobe.load(Ordering::SeqCst);
    }

    pub fn set_drivers_autoprobe(&self, drivers_autoprobe: bool) {
        self.drivers_autoprobe
            .store(drivers_autoprobe, Ordering::SeqCst);
    }

    #[allow(dead_code)]
    #[inline]
    pub fn devices_kset(&self) -> Option<Arc<KSet>> {
        return self.ksets.read().devices_kset.clone();
    }

    #[allow(dead_code)]
    #[inline]
    pub fn set_devices_kset(&self, devices_kset: Arc<KSet>) {
        self.ksets.write().devices_kset = Some(devices_kset);
    }

    #[allow(dead_code)]
    #[inline]
    pub fn drivers_kset(&self) -> Option<Arc<KSet>> {
        return self.ksets.read().drivers_kset.clone();
    }

    pub fn set_drivers_kset(&self, drivers_kset: Arc<KSet>) {
        self.ksets.write().drivers_kset = Some(drivers_kset);
    }

    pub fn bus_notifier(&self) -> &AtomicNotifierChain<BusNotifyEvent, Arc<dyn Device>> {
        return &self.bus_notifier;
    }

    pub fn interfaces(&self) -> &'static [&'static dyn SubSysInterface] {
        return self.interfaces;
    }

    pub fn add_driver_to_vec(&self, driver: &Arc<dyn Driver>) -> Result<(), SystemError> {
        let mut drivers = self.drivers.write();
        if drivers.iter().any(|d| Arc::ptr_eq(d, driver)) {
            return Err(SystemError::EEXIST);
        }
        drivers.push(driver.clone());
        return Ok(());
    }

    pub fn remove_driver_from_vec(&self, driver: &Arc<dyn Driver>) {
        let mut drivers = self.drivers.write();
        let index = drivers.iter().position(|d| Arc::ptr_eq(d, driver));
        if let Some(index) = index {
            drivers.remove(index);
        }
    }

    pub fn add_device_to_vec(&self, device: &Arc<dyn Device>) -> Result<(), SystemError> {
        let mut devices = self.devices.write();
        if devices.iter().any(|d| Arc::ptr_eq(d, device)) {
            return Err(SystemError::EEXIST);
        }
        devices.push(device.clone());
        return Ok(());
    }

    #[allow(dead_code)]
    pub fn remove_device_from_vec(&self, device: &Arc<dyn Device>) {
        let mut devices = self.devices.write();
        let index = devices.iter().position(|d| Arc::ptr_eq(d, device));
        if let Some(index) = index {
            devices.remove(index);
        }
    }
}

/// 参考： https://code.dragonos.org.cn/xref/linux-6.1.9/include/linux/device.h#63
pub trait SubSysInterface: Debug + Send + Sync {
    fn name(&self) -> &str;
    fn bus(&self) -> Option<Weak<dyn Bus>>;
    fn set_bus(&self, bus: Option<Weak<dyn Bus>>);
    fn add_device(&self, _device: &Arc<dyn Device>) -> Result<(), SystemError> {
        return Err(SystemError::ENOSYS);
    }
    fn remove_device(&self, device: &Arc<dyn Device>);
}
