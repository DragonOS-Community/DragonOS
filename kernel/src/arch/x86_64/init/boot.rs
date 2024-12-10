use core::hint::spin_loop;

use system_error::SystemError;

use super::{multiboot2::early_multiboot2_init, pvh::early_linux32_pvh_init};

const BOOT_ENTRY_TYPE_MULTIBOOT: u64 = 1;
const BOOT_ENTRY_TYPE_MULTIBOOT2: u64 = 2;
const BOOT_ENTRY_TYPE_LINUX_32: u64 = 3;
const BOOT_ENTRY_TYPE_LINUX_64: u64 = 4;
const BOOT_ENTRY_TYPE_LINUX_32_PVH: u64 = 5;

#[derive(Debug)]
#[repr(u64)]
enum BootProtocol {
    Multiboot = 1,
    Multiboot2,
    Linux32,
    Linux64,
    Linux32Pvh,
}

impl TryFrom<u64> for BootProtocol {
    type Error = SystemError;

    fn try_from(value: u64) -> Result<Self, Self::Error> {
        match value {
            BOOT_ENTRY_TYPE_MULTIBOOT => Ok(BootProtocol::Multiboot),
            BOOT_ENTRY_TYPE_MULTIBOOT2 => Ok(BootProtocol::Multiboot2),
            BOOT_ENTRY_TYPE_LINUX_32 => Ok(BootProtocol::Linux32),
            BOOT_ENTRY_TYPE_LINUX_64 => Ok(BootProtocol::Linux64),
            BOOT_ENTRY_TYPE_LINUX_32_PVH => Ok(BootProtocol::Linux32Pvh),
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
        BootProtocol::Multiboot2 => early_multiboot2_init(arg1 as u32, arg2),
        BootProtocol::Linux32 | BootProtocol::Linux64 | BootProtocol::Multiboot => loop {
            spin_loop();
        },
        BootProtocol::Linux32Pvh => early_linux32_pvh_init(arg2 as usize),
    }
}
