use crate::syscall::SystemError;

use super::{
    class::classes_init,
    device::{bus::buses_init, init::devices_init},
    firmware::firmware_init,
    hypervisor::hypervisor_init,
};

pub(super) fn driver_init() -> Result<(), SystemError> {
    devices_init()?;
    buses_init()?;
    classes_init()?;
    firmware_init()?;
    hypervisor_init()?;
    return Ok(());
}
