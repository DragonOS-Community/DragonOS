use crate::driver::base::{
    device::{
        bus::{bus_manager, Bus},
        driver::Driver,
        Device,
    },
    subsys::SubSysPrivate,
};
use alloc::{
    string::{String, ToString},
    sync::Arc,
};
use system_error::SystemError;

use super::AcpiManager;

impl AcpiManager {
    /// 通过acpi来匹配驱动
    ///
    /// 参考 https://code.dragonos.org.cn/xref/linux-6.1.9/drivers/acpi/bus.c#949
    pub fn driver_match_device(
        &self,
        _driver: &Arc<dyn Driver>,
        _device: &Arc<dyn Device>,
    ) -> Result<bool, SystemError> {
        // todo:

        return Ok(false);
    }

    /// 参考 https://code.dragonos.org.cn/xref/linux-6.1.9/drivers/acpi/bus.c#1286
    pub(super) fn bus_init(&self) -> Result<(), SystemError> {
        self.acpi_sysfs_init()?;

        let acpi_bus = AcpiBus::new();
        bus_manager()
            .register(acpi_bus as Arc<dyn Bus>)
            .expect("acpi_bus register failed");
        return Ok(());
    }
}

/// ACPI总线
///
/// 参考 https://code.dragonos.org.cn/xref/linux-6.1.9/drivers/acpi/bus.c#1072
#[derive(Debug)]
pub(super) struct AcpiBus {
    private: SubSysPrivate,
}

impl AcpiBus {
    pub fn new() -> Arc<Self> {
        let bus = Arc::new(Self {
            private: SubSysPrivate::new("acpi".to_string(), None, None, &[]),
        });
        bus.subsystem()
            .set_bus(Some(Arc::downgrade(&(bus.clone() as Arc<dyn Bus>))));
        return bus;
    }
}

impl Bus for AcpiBus {
    fn name(&self) -> String {
        return self.private.subsys().as_kobject().name();
    }

    fn dev_name(&self) -> String {
        self.name()
    }

    /// 参考 https://code.dragonos.org.cn/xref/linux-6.1.9/drivers/acpi/bus.c#1056
    fn remove(&self, _device: &Arc<dyn Device>) -> Result<(), SystemError> {
        todo!("acpi_bus: remove")
    }

    fn shutdown(&self, _device: &Arc<dyn Device>) {
        return;
    }

    fn resume(&self, _device: &Arc<dyn Device>) -> Result<(), SystemError> {
        return Ok(());
    }

    /// 通过acpi来匹配驱动
    ///
    /// 参考 https://code.dragonos.org.cn/xref/linux-6.1.9/drivers/acpi/bus.c#1005
    fn match_device(
        &self,
        _device: &Arc<dyn Device>,
        _driver: &Arc<dyn Driver>,
    ) -> Result<bool, SystemError> {
        // todo: 通过acpi来匹配驱动
        return Ok(false);
    }

    /// 参考 https://code.dragonos.org.cn/xref/linux-6.1.9/drivers/acpi/bus.c#1019
    fn probe(&self, _device: &Arc<dyn Device>) -> Result<(), SystemError> {
        todo!("acpi_bus: probe")
    }

    fn subsystem(&self) -> &SubSysPrivate {
        return &self.private;
    }
}

/// Acpi设备应当实现的trait
///
/// 所有的实现了 AcpiDevice trait的结构体，都应该在结构体上方标注`#[cast_to([sync] AcpiDevice)]
///
/// todo: 仿照linux的acpi_device去设计这个trait
///
///
/// 参考 https://code.dragonos.org.cn/xref/linux-6.1.9/include/acpi/acpi_bus.h#364
#[allow(unused)]
pub trait AcpiDevice: Device {}

/// Acpi驱动应当实现的trait
///
/// 所有的实现了 AcpiDriver trait的结构体，都应该在结构体上方标注`#[cast_to([sync] AcpiDriver)]
///
/// todo: 仿照linux的acpi_driver去设计这个trait
///
/// 参考 https://code.dragonos.org.cn/xref/linux-6.1.9/include/acpi/acpi_bus.h#163
#[allow(unused)]
pub trait AcpiDriver: Driver {}
