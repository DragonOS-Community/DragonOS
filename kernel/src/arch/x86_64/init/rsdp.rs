//! Early RSDP discovery for x86 Legacy BIOS boot.
//!
//! This module runs before `mm_init()`, while [`EarlyIoRemap`] is available,
//! and caches only the physical RSDP address (or the discovery error) for the
//! later boot-parameter callback.

use system_error::SystemError;

use crate::{
    arch::MMArch,
    libs::lazy_init::Lazy,
    mm::{early_ioremap::EarlyIoRemap, MemoryManagementArch, PhysAddr},
};

const RSDP_SIGNATURE: &[u8; 8] = b"RSD PTR ";
const RSDP_CHECKSUM_LENGTH: usize = 20;
const RSDP_XCHECKSUM_LENGTH: usize = 36;
const EBDA_PTR_ADDR: usize = 0x40e;
const BIOS_AREA_START: usize = 0xe0000;
const BIOS_AREA_SIZE: usize = 0x20000;
const EBDA_WINDOW_SIZE: usize = 1024;
const LOW_MEMORY_END: usize = 0xa0000;
const RSDP_SCAN_STEP: usize = 16;
const RSDP_MAX_TRAILING_BYTES: usize = RSDP_XCHECKSUM_LENGTH - 1;

/// Result of the single Legacy BIOS discovery attempt performed by the BSP.
///
/// The value is initialized before `mm_init()` and read after `mm_init()`, but
/// always before SMP startup. Bootloader-provided and UEFI paths leave it
/// uninitialized because they must not probe the Legacy BIOS regions.
static BIOS_RSDP_RESULT: Lazy<Result<Option<PhysAddr>, SystemError>> = Lazy::new();

/// Cache one Legacy BIOS discovery result for the later ACPI boot callback.
///
/// This is an early-boot, BSP-only, single-shot operation. The initialized
/// check protects against an accidental repeated call on the same boot path;
/// it is not a general concurrent initialization mechanism.
pub(super) fn cache_bios_rsdp_result() {
    if BIOS_RSDP_RESULT.initialized() {
        return;
    }
    BIOS_RSDP_RESULT.init(find_rsdp_in_bios());
}

/// Return the cached Legacy BIOS discovery result.
///
/// An uninitialized cache means the selected boot path did not request Legacy
/// BIOS discovery (for example, the bootloader supplied an RSDP or MB2 reported
/// an EFI context).
pub(super) fn cached_bios_rsdp() -> Result<Option<PhysAddr>, SystemError> {
    BIOS_RSDP_RESULT.try_get().cloned().unwrap_or(Ok(None))
}

/// Validate an RSDP candidate using the fixed checksum lengths used by ACPICA.
fn rsdp_valid(candidate: &[u8]) -> bool {
    let Some(v1) = candidate.get(..RSDP_CHECKSUM_LENGTH) else {
        return false;
    };

    if &v1[..RSDP_SIGNATURE.len()] != RSDP_SIGNATURE {
        return false;
    }
    if v1.iter().fold(0u8, |sum, byte| sum.wrapping_add(*byte)) != 0 {
        return false;
    }

    let revision = v1[15];
    if revision >= 2 {
        let Some(full) = candidate.get(..RSDP_XCHECKSUM_LENGTH) else {
            return false;
        };
        if full.iter().fold(0u8, |sum, byte| sum.wrapping_add(*byte)) != 0 {
            return false;
        }
    }

    true
}

/// Scan candidates whose start offsets are in `[0, scan_len)`.
fn scan_rsdp_in_buf(
    mapped: &[u8],
    phys_base: usize,
    scan_len: usize,
) -> Result<Option<PhysAddr>, SystemError> {
    let required_len = scan_len
        .checked_add(RSDP_MAX_TRAILING_BYTES)
        .ok_or(SystemError::EINVAL)?;
    if mapped.len() < required_len {
        return Err(SystemError::EINVAL);
    }

    let mut offset = 0usize;
    while offset < scan_len {
        if rsdp_valid(&mapped[offset..]) {
            let candidate_phys = phys_base.checked_add(offset).ok_or(SystemError::EINVAL)?;
            return Ok(Some(PhysAddr::new(candidate_phys)));
        }
        offset += RSDP_SCAN_STEP;
    }

    Ok(None)
}

/// Map and scan one physical search window, then unmap it exactly once.
fn scan_physical_window(
    phys_base: usize,
    scan_len: usize,
) -> Result<Option<PhysAddr>, SystemError> {
    let map_len = scan_len
        .checked_add(RSDP_MAX_TRAILING_BYTES)
        .ok_or(SystemError::EINVAL)?;
    phys_base.checked_add(map_len).ok_or(SystemError::EINVAL)?;
    map_len
        .checked_add(phys_base % MMArch::PAGE_SIZE)
        .ok_or(SystemError::EINVAL)?;

    let virt = EarlyIoRemap::map_not_aligned(PhysAddr::new(phys_base), map_len, true)?;
    // SAFETY: `virt` is the virtual address corresponding to `phys_base`, and
    // `map_not_aligned` successfully mapped at least the requested `map_len`
    // bytes. The slice is used only until the matching unmap below.
    let mapped = unsafe { core::slice::from_raw_parts(virt.data() as *const u8, map_len) };
    let scan_result = scan_rsdp_in_buf(mapped, phys_base, scan_len);
    let unmap_result = EarlyIoRemap::unmap(virt);

    // A cleanup failure indicates a broken early-mapping invariant and takes
    // precedence over a found/not-found scan result.
    unmap_result?;
    scan_result
}

fn read_ebda_base() -> Result<usize, SystemError> {
    let virt = EarlyIoRemap::map_not_aligned(PhysAddr::new(EBDA_PTR_ADDR), 2, true)?;
    // SAFETY: the successful mapping covers both bytes at EBDA_PTR_ADDR. BIOS
    // data is volatile and may not be optimized into ordinary memory reads.
    let segment = unsafe {
        let ptr = virt.data() as *const u8;
        u16::from_le_bytes([
            core::ptr::read_volatile(ptr),
            core::ptr::read_volatile(ptr.add(1)),
        ])
    } as usize;
    EarlyIoRemap::unmap(virt)?;

    Ok(segment << 4)
}

/// Find the RSDP in the ACPI Legacy BIOS search regions.
fn find_rsdp_in_bios() -> Result<Option<PhysAddr>, SystemError> {
    let ebda_phys = read_ebda_base()?;
    if ebda_phys > 0x400 && ebda_phys < LOW_MEMORY_END {
        let scan_len = core::cmp::min(EBDA_WINDOW_SIZE, LOW_MEMORY_END - ebda_phys);
        if let Some(rsdp) = scan_physical_window(ebda_phys, scan_len)? {
            return Ok(Some(rsdp));
        }
    }

    scan_physical_window(BIOS_AREA_START, BIOS_AREA_SIZE)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::vec;

    fn make_rsdp(revision: u8) -> [u8; RSDP_XCHECKSUM_LENGTH] {
        let mut rsdp = [0u8; RSDP_XCHECKSUM_LENGTH];
        rsdp[..8].copy_from_slice(RSDP_SIGNATURE);
        rsdp[9..15].copy_from_slice(b"DRAGON");
        rsdp[15] = revision;
        rsdp[20..24].copy_from_slice(&(RSDP_XCHECKSUM_LENGTH as u32).to_le_bytes());

        let v1_sum = rsdp[..RSDP_CHECKSUM_LENGTH]
            .iter()
            .fold(0u8, |sum, byte| sum.wrapping_add(*byte));
        rsdp[8] = rsdp[8].wrapping_sub(v1_sum);

        let full_sum = rsdp.iter().fold(0u8, |sum, byte| sum.wrapping_add(*byte));
        rsdp[32] = rsdp[32].wrapping_sub(full_sum);
        rsdp
    }

    #[test]
    fn validates_revision_specific_checksums() {
        for revision in [0, 1, 2, 6] {
            assert!(rsdp_valid(&make_rsdp(revision)), "revision {revision}");
        }

        let mut revision_one = make_rsdp(1);
        revision_one[35] ^= 1;
        assert!(rsdp_valid(&revision_one));

        let mut revision_two = make_rsdp(2);
        revision_two[35] ^= 1;
        assert!(!rsdp_valid(&revision_two));
    }

    #[test]
    fn rejects_bad_signature_and_v1_checksum() {
        let mut bad_signature = make_rsdp(2);
        bad_signature[0] ^= 1;
        assert!(!rsdp_valid(&bad_signature));

        let mut bad_checksum = make_rsdp(2);
        bad_checksum[10] ^= 1;
        assert!(!rsdp_valid(&bad_checksum));
    }

    #[test]
    fn scans_first_and_last_candidate_starts() {
        let scan_len = 32usize;
        let mut first = vec![0u8; scan_len + RSDP_MAX_TRAILING_BYTES];
        first[..RSDP_XCHECKSUM_LENGTH].copy_from_slice(&make_rsdp(2));
        assert_eq!(
            scan_rsdp_in_buf(&first, 0x1000, scan_len),
            Ok(Some(PhysAddr::new(0x1000)))
        );

        let mut last = vec![0u8; scan_len + RSDP_MAX_TRAILING_BYTES];
        last[16..16 + RSDP_XCHECKSUM_LENGTH].copy_from_slice(&make_rsdp(2));
        assert_eq!(
            scan_rsdp_in_buf(&last, 0x1000, scan_len),
            Ok(Some(PhysAddr::new(0x1010)))
        );
    }

    #[test]
    fn does_not_scan_candidate_starting_at_scan_len() {
        let scan_len = 16usize;
        let mut mapped = vec![0u8; scan_len + RSDP_XCHECKSUM_LENGTH];
        mapped[scan_len..scan_len + RSDP_XCHECKSUM_LENGTH].copy_from_slice(&make_rsdp(2));
        assert_eq!(scan_rsdp_in_buf(&mapped, 0x1000, scan_len), Ok(None));
    }

    #[test]
    fn rejects_short_mapping_and_physical_address_overflow() {
        assert_eq!(
            scan_rsdp_in_buf(&[0u8; RSDP_XCHECKSUM_LENGTH], 0x1000, 2),
            Err(SystemError::EINVAL)
        );

        let scan_len = 17usize;
        let mut mapped = vec![0u8; scan_len + RSDP_MAX_TRAILING_BYTES];
        mapped[16..16 + RSDP_XCHECKSUM_LENGTH].copy_from_slice(&make_rsdp(2));
        assert_eq!(
            scan_rsdp_in_buf(&mapped, usize::MAX - 15, scan_len),
            Err(SystemError::EINVAL)
        );
    }
}
