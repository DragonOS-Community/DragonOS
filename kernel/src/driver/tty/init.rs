use crate::syscall::SystemError;

use super::serial::serial_early_init;

pub fn tty_early_init() -> Result<(), SystemError> {
    serial_early_init()?;
    return Ok(());
}
