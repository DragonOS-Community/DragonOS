use crate::driver::tty::tty_device::tty_init;
use system_error::SystemError;
use unified_init::{define_public_unified_initializer_slice, unified_init};

use super::{
    class::classes_init,
    cpu::cpu_device_manager,
    device::{bus::buses_init, init::devices_init},
    firmware::firmware_init,
    hypervisor::hypervisor_init,
    platform::platform_bus_init,
};

define_public_unified_initializer_slice!(SUBSYSTEM_INITIALIZER_SLICE);

pub(super) fn driver_init() -> Result<(), SystemError> {
    devices_init()?;
    buses_init()?;
    classes_init()?;
    firmware_init()?;
    hypervisor_init()?;
    platform_bus_init()?;
    cpu_device_manager().init()?;
    subsystem_init()?;
    // 至此，已完成设备驱动模型的初始化
    // 接下来，初始化设备
    actual_device_init()?;
    return Ok(());
}

fn actual_device_init() -> Result<(), SystemError> {
    tty_init()?;

    return Ok(());
}

fn subsystem_init() -> Result<(), SystemError> {
    unified_init!(SUBSYSTEM_INITIALIZER_SLICE);
    return Ok(());
}
