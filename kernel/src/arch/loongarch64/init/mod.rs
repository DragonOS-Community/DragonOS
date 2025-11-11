use system_error::SystemError;

use crate::driver::serial::serial8250::send_to_default_serial8250_port;
pub mod boot;

#[derive(Debug)]
pub struct ArchBootParams {}

impl ArchBootParams {
    pub const DEFAULT: Self = ArchBootParams {};

    pub fn set_alt_mem_k(&mut self, _alt_mem_k: u32) {}

    pub fn set_scratch(&mut self, _scratch: u32) {}

    pub fn init_setupheader(&mut self) {}

    pub fn convert_to_buf(&self) -> &[u8] {
        unsafe {
            core::slice::from_raw_parts(
                (self as *const Self) as *const u8,
                core::mem::size_of::<Self>(),
            )
        }
    }
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
