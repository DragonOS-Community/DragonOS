use system_error::SystemError;

use crate::arch::init::multiboot::early_multiboot_init;

use super::multiboot2::early_multiboot2_init;

const BOOT_ENTRY_TYPE_MULTIBOOT: u64 = 1;
const BOOT_ENTRY_TYPE_MULTIBOOT2: u64 = 2;
const BOOT_ENTRY_TYPE_LINUX_32: u64 = 3;
const BOOT_ENTRY_TYPE_LINUX_64: u64 = 4;

#[derive(Debug)]
#[repr(u64)]
enum BootProtocol {
    Multiboot = 1,
    Multiboot2,
    Linux32,
    Linux64,
}

impl TryFrom<u64> for BootProtocol {
    type Error = SystemError;

    fn try_from(value: u64) -> Result<Self, Self::Error> {
        match value {
            BOOT_ENTRY_TYPE_MULTIBOOT => Ok(BootProtocol::Multiboot),
            BOOT_ENTRY_TYPE_MULTIBOOT2 => Ok(BootProtocol::Multiboot2),
            BOOT_ENTRY_TYPE_LINUX_32 => Ok(BootProtocol::Linux32),
            BOOT_ENTRY_TYPE_LINUX_64 => Ok(BootProtocol::Linux64),
            _ => Err(SystemError::EINVAL),
        }
    }
}

#[inline(never)]
pub(super) fn early_boot_init(
    boot_entry_type: u64,
    arg1: u64,
    arg2: u64,
) -> Result<(), SystemError> {
    let boot_protocol = BootProtocol::try_from(boot_entry_type)?;
    match boot_protocol {
        BootProtocol::Multiboot => early_multiboot_init(arg1 as u32, arg2),
        BootProtocol::Multiboot2 => early_multiboot2_init(arg1 as u32, arg2),
        BootProtocol::Linux32 => {
            // linux32_init(arg1, arg2);
            unimplemented!();
        }
        BootProtocol::Linux64 => {
            // linux64_init(arg1, arg2);
            unimplemented!();
        }
    }
}
