use alloc::string::String;
use system_error::SystemError;

use crate::{
    driver::video::fbdev::base::BootTimeScreenInfo,
    init::boot::{register_boot_callbacks, BootCallbacks, BootloaderAcpiArg},
};

pub(super) fn early_dragonstub_init() -> Result<(), SystemError> {
    register_boot_callbacks(&DragonStubCallBack);
    Ok(())
}

struct DragonStubCallBack;

impl BootCallbacks for DragonStubCallBack {
    fn init_bootloader_name(&self) -> Result<Option<String>, SystemError> {
        Ok(format!("DragonStub").into())
    }

    fn init_acpi_args(&self) -> Result<BootloaderAcpiArg, SystemError> {
        Ok(BootloaderAcpiArg::NotProvided)
    }

    fn init_kernel_cmdline(&self) -> Result<(), SystemError> {
        // parsed in `early_init_scan_chosen()`
        Ok(())
    }

    fn early_init_framebuffer_info(
        &self,
        _scinfo: &mut BootTimeScreenInfo,
    ) -> Result<(), SystemError> {
        unimplemented!("dragonstub early_init_framebuffer_info")
    }

    fn early_init_memory_blocks(&self) -> Result<(), SystemError> {
        // parsed in `early_init_scan_memory()` and uefi driver
        Ok(())
    }

    fn early_init_memmap_sysfs(&self) -> Result<(), SystemError> {
        log::error!("riscv64, early_init_memmap_sysfs is not impled");
        Ok(())
    }

    fn init_initramfs(&self) -> Result<(), SystemError> {
        log::error!("riscv64, init_initramfs is not impled");
        Ok(())
    }

    fn init_memmap_bp(&self) -> Result<(), SystemError> {
        log::error!("riscv64, init_memmap_bp is not impled");
        Ok(())
    }
}
