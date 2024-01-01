use system_error::SystemError;

use super::{
    class::classes_init,
    cpu::cpu_device_manager,
    device::{bus::buses_init, init::devices_init},
    firmware::firmware_init,
    hypervisor::hypervisor_init,
    platform::platform_bus_init,
};

pub(super) fn driver_init() -> Result<(), SystemError> {
    devices_init()?;
    buses_init()?;
    classes_init()?;
    firmware_init()?;
    hypervisor_init()?;
    platform_bus_init()?;
    cpu_device_manager().init()?;

    // 至此，已完成设备驱动模型的初始化
    // 接下来，初始化设备
    actual_device_init()?;
    return Ok(());
}

fn actual_device_init() -> Result<(), SystemError> {
    // 应当使用unified_init来初始化
    return Ok(());
}
