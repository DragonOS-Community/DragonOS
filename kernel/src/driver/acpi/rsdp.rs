//! RSDP (Root System Description Pointer) discovery.
//!
//! When the bootloader does not hand us an RSDP physical address, we fall back
//! to scanning the well-known BIOS locations, following the ACPI specification.
//!
//! Reference: Linux `acpi_find_root_pointer()` in drivers/acpi/acpica/tbxfroot.c
//! Also see: https://wiki.osdev.org/RSDP

use crate::mm::{PhysAddr, early_ioremap::EarlyIoRemap};

// On IA-PC systems, the RSDP is either located within the first 1 KiB of
// the EBDA or in the memory region from 0x000E0000 to 0x000FFFFF.
// To find the table, the operating system has to find the RSD PTR signature (notice the last space character) in one of the two areas.
// The signature always starts on a 16 byte boundary. - wiki.osdev.org

/// RSDP signature: An 8-byte magic number "RSD PTR"
const RSDP_SIGNATURE: &[u8; 8] = b"RSD PTR ";
/// Physical address of the 2-byte EBDA segment pointer in the BIOS data area.
const EBDA_PTR_ADDR: usize = 0x40E;
/// Start of the BIOS read-only memory region.
const BIOS_AREA_START: usize = 0xE0000;
/// Size of the BIOS read-only memory region (0xE0000 - 0xFFFFF).
const BIOS_AREA_SIZE: usize = 0x20000;

/// Validate an RSDP structure at the given virtual address.
///
/// Checks the 8-byte signature, v1 checksum (first 20 bytes), and if
/// revision >= 2 also verifies the extended checksum over the full length.
/// ported from linux kernel: `acpi_tb_validate_rsdp()`
fn rsdp_valid(ptr: *const u8, available_len: usize) -> bool {
    // SAFETY: every check will check available_len fist.
    if available_len < 20 {
        return false;
    }
    // check the RSDP_SIGNATURE
    let sig = unsafe { core::slice::from_raw_parts(ptr, 8) };
    if sig != RSDP_SIGNATURE {
        return false;
    }
    // ACPI v1 checksum covers the first 20 bytes
    let v1 = unsafe { core::slice::from_raw_parts(ptr, 20) };
    if v1.iter().fold(0u8, |acc, &b| acc.wrapping_add(b)) != 0 {
        return false;
    }
    // ACPI 2.0+ if revision >= 2, also verify the extended checksum over the full struct
    let revision = unsafe { *ptr.add(15) };
    if revision >= 2 {
        if available_len < 36 {
            return false;
        }
        let length =
            unsafe { u32::from_le_bytes([*ptr.add(20), *ptr.add(21), *ptr.add(22), *ptr.add(23)]) }
                as usize;
        // reject candidates whose extended checksum cannot be fully validated:
        // a corrupted/out-of-range length must not pass as a valid RSDP.
        if length < 36 || length > available_len {
            return false;
        }
        let full = unsafe { core::slice::from_raw_parts(ptr, length) };
        if full.iter().fold(0u8, |acc, &b| acc.wrapping_add(b)) != 0 {
            return false;
        }
    }
    true
}

/// Scan a virtually-mapped buffer for a valid RSDP, stepping 16 bytes at a time.
/// Returns the physical address of the RSDP if found.
fn scan_rsdp_in_buf(virt_base: usize, phys_base: usize, size: usize) -> Option<PhysAddr> {
    let mut offset = 0usize;
    while offset + 20 <= size {
        let ptr = (virt_base + offset) as *const u8;
        // scan rsdp
        if rsdp_valid(ptr, size - offset) {
            return Some(PhysAddr::new(phys_base + offset));
        }
        offset += 16;
    }
    None
}

/// Discover the RSDP by scanning the standard BIOS locations.
///
/// Searches the EBDA (first 1 KiB) and the BIOS read-only area
/// (0xE0000 - 0xFFFFF), using [`EarlyIoRemap`] for temporary physical access.
///
// SAFETY: Safe to call after `mm_init()`; [`EarlyIoRemap`] uses the current CR3 and
// allocates page-table pages from the static BSS pool.
pub fn find_rsdp_in_bios() -> Option<PhysAddr> {
    // First search the EBDA area
    // Read the 2-byte segment pointer at 0x40E, shift left 4 for its physical address.
    if let Ok(ptr_virt) = EarlyIoRemap::map_not_aligned(PhysAddr::new(EBDA_PTR_ADDR), 2, true) {
        let ebda_segment = unsafe {
            let ptr = ptr_virt.data() as *const u8;

            u16::from_le_bytes([
                core::ptr::read_volatile(ptr),
                core::ptr::read_volatile(ptr.add(1)),
            ])
        } as usize;
        let _ = EarlyIoRemap::unmap(ptr_virt);

        let ebda_phys = ebda_segment << 4;
        if ebda_phys > 0x400 && ebda_phys < 0xA0000 {
            if let Ok(ebda_virt) =
                // first 1kb of EBDA
                EarlyIoRemap::map_not_aligned(PhysAddr::new(ebda_phys), 1024, true)
            {
                // scan rsdp
                let result = scan_rsdp_in_buf(ebda_virt.data(), ebda_phys, 1024);
                let _ = EarlyIoRemap::unmap(ebda_virt);
                if result.is_some() {
                    return result;
                }
            }
        }
    }

    // Second search the BIOS area
    if let Ok(bios_virt) =
        EarlyIoRemap::map_not_aligned(PhysAddr::new(BIOS_AREA_START), BIOS_AREA_SIZE, true)
    {
        let result = scan_rsdp_in_buf(bios_virt.data(), BIOS_AREA_START, BIOS_AREA_SIZE);
        let _ = EarlyIoRemap::unmap(bios_virt);
        if result.is_some() {
            return result;
        }
    }

    None
}
