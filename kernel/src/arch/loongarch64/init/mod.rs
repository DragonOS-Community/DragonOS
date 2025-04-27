use system_error::SystemError;
pub mod boot;

#[derive(Debug)]
pub struct ArchBootParams {}

impl ArchBootParams {
    pub const DEFAULT: Self = ArchBootParams {};
}

#[inline(never)]
pub fn early_setup_arch() -> Result<(), SystemError> {
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
