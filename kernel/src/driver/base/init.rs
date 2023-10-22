use crate::{driver::tty::tty_device::tty_init, syscall::SystemError};

use super::{
    class::classes_init,
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

    // 至此，已完成设备驱动模型的初始化
    // 接下来，初始化设备
    actual_device_init()?;
    return Ok(());
}

fn actual_device_init() -> Result<(), SystemError> {
    tty_init()?;

    return Ok(());
}
