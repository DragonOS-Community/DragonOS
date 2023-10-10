use core::{
    fmt::Debug,
    sync::atomic::{AtomicBool, Ordering},
};

use alloc::{
    string::String,
    sync::{Arc, Weak},
    vec::Vec,
};

use crate::{
    driver::Driver,
    libs::{notifier::AtomicNotifierChain, rwlock::RwLock, spinlock::SpinLock},
    syscall::SystemError,
};

use super::{
    device::{
        bus::{Bus, BusNotifyEvent},
        Device,
    },
    kset::KSet,
};

/// 一个用于存储bus/class的驱动核心部分的信息的结构体
#[derive(Debug)]
pub struct SubSysPrivate {
    /// 用于定义这个子系统的kset
    subsys: Arc<KSet>,
    ksets: RwLock<SubSysKSets>,
    /// 指向拥有当前结构体的`dyn bus`对象的弱引用
    bus: SpinLock<Weak<dyn Bus>>,
    drivers_autoprobe: AtomicBool,
    /// 当前总线上的所有设备
    devices: RwLock<Vec<Weak<dyn Device>>>,
    /// 当前总线上的所有驱动
    drivers: RwLock<Vec<Weak<dyn Driver>>>,
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
        bus: Weak<dyn Bus>,
        interfaces: &'static [&'static dyn SubSysInterface],
    ) -> Self {
        let subsys = KSet::new(name);
        return Self {
            subsys,
            ksets: RwLock::new(SubSysKSets::new()),
            drivers_autoprobe: AtomicBool::new(false),
            bus: SpinLock::new(bus),
            devices: RwLock::new(Vec::new()),
            drivers: RwLock::new(Vec::new()),
            interfaces,
            bus_notifier: AtomicNotifierChain::new(),
        };
    }

    pub fn subsys(&self) -> Arc<KSet> {
        return self.subsys.clone();
    }

    pub fn bus(&self) -> Weak<dyn Bus> {
        return self.bus.lock().clone();
    }

    pub fn set_bus(&self, bus: Weak<dyn Bus>) {
        *self.bus.lock() = bus;
    }

    pub fn devices(&self) -> &RwLock<Vec<Weak<dyn Device>>> {
        return &self.devices;
    }

    pub fn drivers(&self) -> &RwLock<Vec<Weak<dyn Driver>>> {
        return &self.drivers;
    }

    pub fn drivers_autoprobe(&self) -> bool {
        return self.drivers_autoprobe.load(Ordering::SeqCst);
    }

    pub fn set_drivers_autoprobe(&self, drivers_autoprobe: bool) {
        self.drivers_autoprobe
            .store(drivers_autoprobe, Ordering::SeqCst);
    }

    pub fn devices_kset(&self) -> Option<Arc<KSet>> {
        return self.ksets.read().devices_kset.clone();
    }

    pub fn set_devices_kset(&self, devices_kset: Arc<KSet>) {
        self.ksets.write().devices_kset = Some(devices_kset);
    }

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
}

/// 参考： https://opengrok.ringotek.cn/xref/linux-6.1.9/include/linux/device.h#63
pub trait SubSysInterface: Debug + Send + Sync {
    fn name(&self) -> &str;
    fn bus(&self) -> Option<Weak<dyn Bus>>;
    fn set_bus(&self, bus: Option<Weak<dyn Bus>>);
    fn add_device(&self, _device: &Arc<dyn Device>) -> Result<(), SystemError> {
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }
    fn remove_device(&self, device: &Arc<dyn Device>);
}
