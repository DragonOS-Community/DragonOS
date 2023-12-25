use super::serial::serial_early_init;
use system_error::SystemError;

pub fn tty_early_init() -> Result<(), SystemError> {
    serial_early_init()?;
    return Ok(());
}
