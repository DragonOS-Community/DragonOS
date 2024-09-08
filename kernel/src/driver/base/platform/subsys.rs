use alloc::{
    string::{String, ToString},
    sync::{Arc, Weak},
};
use intertrait::cast::CastArc;
use log::error;

use super::{
    platform_bus_device, platform_device::PlatformDevice, platform_driver::PlatformDriver,
};
use crate::{
    driver::{
        acpi::acpi_manager,
        base::{
            device::{bus::Bus, driver::Driver, Device},
            kobject::KObject,
            subsys::SubSysPrivate,
        },
    },
    filesystem::{
        sysfs::{Attribute, AttributeGroup},
        vfs::syscall::ModeType,
    },
};
use system_error::SystemError;

#[derive(Debug)]
pub struct PlatformBus {
    private: SubSysPrivate,
}

impl PlatformBus {
    pub fn new() -> Arc<Self> {
        let w: Weak<Self> = Weak::new();
        let private = SubSysPrivate::new("platform".to_string(), Some(w), None, &[]);
        let bus = Arc::new(Self { private });
        bus.subsystem()
            .set_bus(Some(Arc::downgrade(&(bus.clone() as Arc<dyn Bus>))));

        return bus;
    }
}

impl Bus for PlatformBus {
    fn name(&self) -> String {
        return "platform".to_string();
    }

    fn dev_name(&self) -> String {
        return self.name();
    }

    fn dev_groups(&self) -> &'static [&'static dyn AttributeGroup] {
        return &[&PlatformDeviceAttrGroup];
    }

    fn subsystem(&self) -> &SubSysPrivate {
        return &self.private;
    }

    fn probe(&self, device: &Arc<dyn Device>) -> Result<(), SystemError> {
        let drv = device.driver().ok_or(SystemError::EINVAL)?;
        let pdrv = drv.cast::<dyn PlatformDriver>().map_err(|_|{
            error!("PlatformBus::probe() failed: device.driver() is not a PlatformDriver. Device: '{:?}'", device.name());
            SystemError::EINVAL
        })?;

        let pdev = device.clone().cast::<dyn PlatformDevice>().map_err(|_| {
            error!(
                "PlatformBus::probe() failed: device is not a PlatformDevice. Device: '{:?}'",
                device.name()
            );
            SystemError::EINVAL
        })?;

        return pdrv.probe(&pdev);
    }

    fn remove(&self, _device: &Arc<dyn Device>) -> Result<(), SystemError> {
        todo!()
    }

    fn sync_state(&self, _device: &Arc<dyn Device>) {
        todo!()
    }

    fn shutdown(&self, _device: &Arc<dyn Device>) {
        todo!()
    }

    fn resume(&self, _device: &Arc<dyn Device>) -> Result<(), SystemError> {
        todo!()
    }

    ///
    /// match platform device to platform driver.
    ///
    /// ## 参数
    ///
    /// * `device` - platform device
    /// * `driver` - platform driver
    ///
    /// ## 返回
    ///
    /// - `Ok(true)` - 匹配成功
    /// - `Ok(false)` - 匹配失败
    /// - `Err(_)` - 由于内部错误导致匹配失败
    ///
    /// Platform device IDs are assumed to be encoded like this:
    /// "<name><instance>", where <name> is a short description of the type of
    /// device, like "pci" or "floppy", and <instance> is the enumerated
    /// instance of the device, like '0' or '42'.  Driver IDs are simply
    /// "<name>".  So, extract the <name> from the platform_device structure,
    /// and compare it against the name of the driver. Return whether they match
    /// or not.
    ///
    /// 参考 https://code.dragonos.org.cn/xref/linux-6.1.9/drivers/base/platform.c#1331
    ///
    ///
    fn match_device(
        &self,
        device: &Arc<dyn Device>,
        driver: &Arc<dyn Driver>,
    ) -> Result<bool, SystemError> {
        // 尝试从 ACPI 中匹配
        if let Ok(x) = acpi_manager().driver_match_device(driver, device) {
            if x {
                return Ok(true);
            }
        }

        // 尝试从 ID table 中匹配
        if let Some(drv_id_table) = driver.id_table() {
            let pdev = device
                .clone()
                .cast::<dyn PlatformDevice>()
                .map_err(|_| SystemError::EINVAL)?;
            if drv_id_table.name().eq(&pdev.name()) {
                return Ok(true);
            }
        }

        // 尝试根据设备名称匹配
        return Ok(device.name().eq(&driver.name()));
    }

    fn root_device(&self) -> Option<Weak<dyn Device>> {
        let root_device = platform_bus_device() as Arc<dyn Device>;
        return Some(Arc::downgrade(&root_device));
    }
}

#[derive(Debug)]
pub struct PlatformDeviceAttrGroup;

impl AttributeGroup for PlatformDeviceAttrGroup {
    fn name(&self) -> Option<&str> {
        None
    }

    fn attrs(&self) -> &[&'static dyn Attribute] {
        // todo: https://code.dragonos.org.cn/xref/linux-6.1.9/drivers/base/platform.c?r=&mo=38425&fi=1511#1311
        return &[];
    }

    fn is_visible(&self, _kobj: Arc<dyn KObject>, attr: &dyn Attribute) -> Option<ModeType> {
        return Some(attr.mode());
    }
}
