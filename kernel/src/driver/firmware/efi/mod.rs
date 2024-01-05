mod fdt;
pub mod init;

#[derive(Debug)]
pub struct EFIManager;

#[inline(always)]
pub fn efi_manager() -> &'static EFIManager {
    &EFIManager
}
