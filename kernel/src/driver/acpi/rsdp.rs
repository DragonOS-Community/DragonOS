//! RSDP (Root System Description Pointer) discovery.
//!
//! When the bootloader does not hand us an RSDP physical address, we fall back
//! to scanning the well-known BIOS locations, following the ACPI specification.
//!
//! Reference: Linux `acpi_find_root_pointer()` in drivers/acpi/acpica/tbxfroot.c
//! Also see: https://wiki.osdev.org/RSDP

use crate::mm::{early_ioremap::EarlyIoRemap, PhysAddr};

// On IA-PC systems, the RSDP is either located within the first 1 KiB of
// the EBDA or in the memory region from 0x000E0000 to 0x000FFFFF.
// To find the table, the operating system has to find the RSD PTR signature (notice the last space character) in one of the two areas.
// The signature always starts on a 16 byte boundary. - wiki.osdev.org

/// RSDP signature: An 8-byte magic number "RSD PTR"
const RSDP_SIGNATURE: &[u8; 8] = b"RSD PTR ";
/// ACPI 1.0 checksum length (Linux: ACPI_RSDP_CHECKSUM_LENGTH)
const RSDP_CHECKSUM_LENGTH: usize = 20;
/// ACPI 2.0+ extended checksum length (Linux: ACPI_RSDP_XCHECKSUM_LENGTH)
const RSDP_XCHECKSUM_LENGTH: usize = 36;
/// Physical address of the 2-byte EBDA segment pointer in the BIOS data area.
const EBDA_PTR_ADDR: usize = 0x40E;
/// Start of the BIOS read-only memory region.
const BIOS_AREA_START: usize = 0xE0000;
/// Size of the BIOS read-only memory region (0xE0000 - 0xFFFFF).
const BIOS_AREA_SIZE: usize = 0x20000;
/// Default EBDA scan window: the first 1 KiB of the EBDA.
const EBDA_WINDOW_SIZE: usize = 1024;
/// End of conventional low memory; the VGA/ISA reserved region begins here.
const LOW_MEMORY_END: usize = 0xA0000;
/// Scan step: 16-byte boundary alignment (Linux: ACPI_RSDP_SCAN_STEP)
const RSDP_SCAN_STEP: usize = 16;

/// Validate an RSDP structure at the given virtual address.
///
/// Checks the 8-byte signature, v1 checksum (first 20 bytes), and if
/// revision >= 2 also verifies the extended checksum (fixed 36 bytes).
///
/// Follows Linux `acpi_tb_validate_rsdp()` in drivers/acpi/acpica/tbxfroot.c:
/// - Never uses the firmware-provided `length` field for checksum computation.
/// - revision 0/1: only v1 checksum (20 bytes).
/// - revision >= 2: v1 checksum (20 bytes) + extended checksum (36 bytes).
fn rsdp_valid(ptr: *const u8, available_len: usize) -> bool {
    if available_len < RSDP_CHECKSUM_LENGTH {
        return false;
    }
    let sig = unsafe { core::slice::from_raw_parts(ptr, 8) };
    if sig != RSDP_SIGNATURE {
        return false;
    }
    let v1 = unsafe { core::slice::from_raw_parts(ptr, RSDP_CHECKSUM_LENGTH) };
    if v1.iter().fold(0u8, |acc, &b| acc.wrapping_add(b)) != 0 {
        return false;
    }
    let revision = unsafe { *ptr.add(15) };
    if revision >= 2 {
        if available_len < RSDP_XCHECKSUM_LENGTH {
            return false;
        }
        let full = unsafe { core::slice::from_raw_parts(ptr, RSDP_XCHECKSUM_LENGTH) };
        if full.iter().fold(0u8, |acc, &b| acc.wrapping_add(b)) != 0 {
            return false;
        }
    }
    true
}

/// Scan a virtually-mapped buffer for a valid RSDP, stepping 16 bytes at a time.
///
/// `scan_len` is the candidate search range (candidate start must be < scan_len).
/// `mapped_len` is the total readable bytes from virt_base (must be >= scan_len + 35
/// to guarantee the last candidate's full 36 bytes are readable).
///
/// Follows Linux `acpi_tb_scan_memory_for_rsdp()`: the loop condition only requires
/// the candidate start to be within the scan range.
fn scan_rsdp_in_buf(
    virt_base: usize,
    phys_base: usize,
    scan_len: usize,
    mapped_len: usize,
) -> Option<PhysAddr> {
    let mut offset = 0usize;
    while offset < scan_len {
        let ptr = (virt_base + offset) as *const u8;
        if rsdp_valid(ptr, mapped_len - offset) {
            return Some(PhysAddr::new(phys_base + offset));
        }
        offset += RSDP_SCAN_STEP;
    }
    None
}

/// Discover the RSDP by scanning the standard BIOS locations.
///
/// Searches the EBDA (first 1 KiB) and the BIOS read-only area
/// (0xE0000 - 0xFFFFF), using [`EarlyIoRemap`] for temporary physical access.
///
/// Must be called before `mm_init()` while the bootstrap page table is active.
/// [`EarlyIoRemap`] uses the current CR3 and allocates page-table pages from
/// the static BSS pool (`PseudoAllocator`), which must not be used after mm_init.
pub fn find_rsdp_in_bios() -> Option<PhysAddr> {
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
        if ebda_phys > 0x400 && ebda_phys < LOW_MEMORY_END {
            let scan_len = core::cmp::min(EBDA_WINDOW_SIZE, LOW_MEMORY_END - ebda_phys);
            let map_len = scan_len + RSDP_XCHECKSUM_LENGTH - 1;
            if let Ok(ebda_virt) =
                EarlyIoRemap::map_not_aligned(PhysAddr::new(ebda_phys), map_len, true)
            {
                let result = scan_rsdp_in_buf(ebda_virt.data(), ebda_phys, scan_len, map_len);
                let _ = EarlyIoRemap::unmap(ebda_virt);
                if result.is_some() {
                    return result;
                }
            }
        }
    }

    let scan_len = BIOS_AREA_SIZE;
    let map_len = scan_len + RSDP_XCHECKSUM_LENGTH - 1;
    if let Ok(bios_virt) =
        EarlyIoRemap::map_not_aligned(PhysAddr::new(BIOS_AREA_START), map_len, true)
    {
        let result = scan_rsdp_in_buf(bios_virt.data(), BIOS_AREA_START, scan_len, map_len);
        let _ = EarlyIoRemap::unmap(bios_virt);
        if result.is_some() {
            return result;
        }
    }

    None
}
