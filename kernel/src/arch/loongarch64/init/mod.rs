use system_error::SystemError;

use crate::driver::serial::serial8250::send_to_default_serial8250_port;
pub mod boot;

#[derive(Debug)]
pub struct ArchBootParams {}

impl ArchBootParams {
    pub const DEFAULT: Self = ArchBootParams {};
}

#[inline(never)]
pub fn early_setup_arch() -> Result<(), SystemError> {
    send_to_default_serial8250_port(b"la64:early_setup_arch");
    loop {}
    todo!("la64:early_setup_arch");
}

#[inline(never)]
pub fn setup_arch() -> Result<(), SystemError> {
    todo!("la64:setup_arch");
}

#[inline(never)]
pub fn setup_arch_post() -> Result<(), SystemError> {
    todo!("la64:setup_arch_post");
}
