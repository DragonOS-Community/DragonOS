use alloc::{
    string::{String, ToString},
    sync::{Arc, Weak},
};
use intertrait::cast::CastArc;
use log::error;
use system_error::SystemError;

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

use super::{serio_device::SerioDevice, serio_driver::SerioDriver};

#[derive(Debug)]
pub struct SerioBus {
    private: SubSysPrivate,
}

impl SerioBus {
    pub fn new() -> Arc<Self> {
        let w: Weak<Self> = Weak::new();
        let private = SubSysPrivate::new("serio".to_string(), Some(w), None, &[]);
        let bus = Arc::new(Self { private });
        bus.subsystem()
            .set_bus(Some(Arc::downgrade(&(bus.clone() as Arc<dyn Bus>))));

        return bus;
    }
}

impl Bus for SerioBus {
    fn name(&self) -> String {
        return "serio".to_string();
    }

    fn dev_name(&self) -> String {
        return self.name();
    }

    fn dev_groups(&self) -> &'static [&'static dyn AttributeGroup] {
        return &[&SerioDeviceAttrGroup];
    }

    fn subsystem(&self) -> &SubSysPrivate {
        return &self.private;
    }

    fn probe(&self, device: &Arc<dyn Device>) -> Result<(), SystemError> {
        let drv = device.driver().ok_or(SystemError::EINVAL)?;
        let pdrv = drv.cast::<dyn SerioDriver>().map_err(|_| {
            error!(
                "SerioBus::probe() failed: device.driver() is not a SerioDriver. Device: '{:?}'",
                device.name()
            );
            SystemError::EINVAL
        })?;

        let pdev = device.clone().cast::<dyn SerioDevice>().map_err(|_| {
            error!(
                "SerioBus::probe() failed: device is not a SerioDevice. Device: '{:?}'",
                device.name()
            );
            SystemError::EINVAL
        })?;

        return pdrv.connect(&pdev);
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
                .cast::<dyn SerioDevice>()
                .map_err(|_| SystemError::EINVAL)?;
            if drv_id_table.name().eq(&pdev.name()) {
                return Ok(true);
            }
        }

        // 尝试根据设备名称匹配
        return Ok(device.name().eq(&driver.name()));
    }
}

#[derive(Debug)]
pub struct SerioDeviceAttrGroup;

impl AttributeGroup for SerioDeviceAttrGroup {
    fn name(&self) -> Option<&str> {
        None
    }

    /// todo: https://code.dragonos.org.cn/xref/linux-6.1.9/drivers/input/serio/serio.c#473
    fn attrs(&self) -> &[&'static dyn Attribute] {
        return &[];
    }

    fn is_visible(&self, _kobj: Arc<dyn KObject>, _attr: &dyn Attribute) -> Option<ModeType> {
        None
    }
}
