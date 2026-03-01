mod device;

use alloc::{string::ToString, sync::Arc, vec::Vec};
use core::convert::TryFrom;
use system_error::SystemError;
use unified_init::macros::unified_init;

use crate::{
    driver::base::block::{block_device::BlockDevice, manager::block_dev_manager},
    init::initcall::INITCALL_DEVICE,
};

use self::device::PmemBlockDevice;

const E820_TYPE_PMEM: u32 = 7;
const E820_TYPE_PRAM: u32 = 12;
const MAX_E820_ENTRIES: usize = 128;
const PMEM_BLOCK_SIZE: usize = crate::driver::base::block::block_device::LBA_SIZE;

#[cfg(target_arch = "x86_64")]
const NFIT_SUBTABLE_HEADER_SIZE: usize = 4;
#[cfg(target_arch = "x86_64")]
const NFIT_SUBTABLE_TYPE_SYSTEM_ADDRESS: u16 = 0;
#[cfg(target_arch = "x86_64")]
const NFIT_SUBTABLE_TYPE_MEMORY_MAP: u16 = 1;
#[cfg(target_arch = "x86_64")]
const NFIT_SYSTEM_ADDRESS_MIN_LEN: usize = 56;
#[cfg(target_arch = "x86_64")]
const NFIT_MEMORY_MAP_LEN: usize = 48;
#[cfg(target_arch = "x86_64")]
const ACPI_NFIT_MEM_MAP_FAILED: u16 = 1 << 6;
#[cfg(target_arch = "x86_64")]
const NFIT_GUID_PERSISTENT_MEMORY: [u8; 16] = [
    0x79, 0xd3, 0xf0, 0x66, 0xf3, 0xb4, 0x74, 0x40, 0xac, 0x43, 0x0d, 0x33, 0x18, 0xb7, 0x8c, 0xdb,
];

#[cfg(target_arch = "x86_64")]
#[repr(C, packed)]
struct NfitTable {
    header: acpi::sdt::SdtHeader,
    reserved: u32,
}

#[cfg(target_arch = "x86_64")]
unsafe impl acpi::AcpiTable for NfitTable {
    const SIGNATURE: acpi::sdt::Signature = acpi::sdt::Signature::NFIT;

    fn header(&self) -> &acpi::sdt::SdtHeader {
        &self.header
    }
}

#[cfg(target_arch = "x86_64")]
#[derive(Debug, Clone, Copy)]
struct NfitSpaRange {
    range_index: u16,
    address: u64,
    length: u64,
    is_pmem: bool,
}

#[cfg(target_arch = "x86_64")]
#[derive(Debug, Clone, Copy)]
struct NfitMemMapRange {
    range_index: u16,
    address: u64,
    region_size: u64,
    flags: u16,
}

#[inline(always)]
fn trim_to_block_aligned(size: usize) -> usize {
    size / PMEM_BLOCK_SIZE * PMEM_BLOCK_SIZE
}

#[cfg(target_arch = "x86_64")]
#[inline(always)]
fn read_u16_le(bytes: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        bytes.get(offset..offset + 2)?.try_into().ok()?,
    ))
}

#[cfg(target_arch = "x86_64")]
#[inline(always)]
fn read_u64_le(bytes: &[u8], offset: usize) -> Option<u64> {
    Some(u64::from_le_bytes(
        bytes.get(offset..offset + 8)?.try_into().ok()?,
    ))
}

#[cfg(target_arch = "x86_64")]
fn normalize_regions(
    mut regions: Vec<(crate::mm::PhysAddr, usize)>,
) -> Vec<(crate::mm::PhysAddr, usize)> {
    regions.sort_by_key(|(start, _)| start.data());
    regions.dedup_by(|a, b| a.0 == b.0 && a.1 == b.1);
    regions
}

#[cfg(target_arch = "x86_64")]
fn collect_pmem_regions_from_e820() -> Vec<(crate::mm::PhysAddr, usize)> {
    use crate::{init::boot_params, mm::PhysAddr};

    let mut regions = Vec::new();
    let bp = boot_params().read();

    for idx in 0..MAX_E820_ENTRIES {
        let entry = bp.arch.e820_table[idx];
        let entry_addr = entry.addr;
        let entry_size = entry.size;
        let entry_type = entry.type_;
        if (entry_type != E820_TYPE_PMEM && entry_type != E820_TYPE_PRAM) || entry_size == 0 {
            continue;
        }

        let start = match usize::try_from(entry_addr) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let size = match usize::try_from(entry_size) {
            Ok(v) => v,
            Err(_) => continue,
        };

        // BlockDevice 以 512B 粒度导出，尾部不足 512B 的部分截断。
        let size = trim_to_block_aligned(size);
        if size == 0 {
            continue;
        }

        regions.push((PhysAddr::new(start), size));
    }

    let regions = normalize_regions(regions);
    regions
}

#[cfg(target_arch = "x86_64")]
fn collect_pmem_regions_from_nfit() -> Vec<(crate::mm::PhysAddr, usize)> {
    use crate::{driver::acpi::acpi_manager, mm::PhysAddr};

    let mut spa_ranges: Vec<NfitSpaRange> = Vec::new();
    let mut memmap_ranges: Vec<NfitMemMapRange> = Vec::new();

    let Some(acpi_tables) = acpi_manager().tables() else {
        return Vec::new();
    };

    let nfit_table = match acpi_tables.find_entire_table::<NfitTable>() {
        Ok(table) => table,
        Err(_) => {
            return Vec::new();
        }
    };

    let bytes = unsafe {
        core::slice::from_raw_parts(
            nfit_table.virtual_start().as_ptr(),
            nfit_table.region_length(),
        )
    };

    let table_len = bytes.len();
    if table_len < core::mem::size_of::<NfitTable>() {
        return Vec::new();
    }

    let mut offset = core::mem::size_of::<NfitTable>();
    while offset + NFIT_SUBTABLE_HEADER_SIZE <= table_len {
        let entry_type = match read_u16_le(bytes, offset) {
            Some(v) => v,
            None => break,
        };
        let entry_len = match read_u16_le(bytes, offset + 2) {
            Some(v) => v as usize,
            None => break,
        };

        if entry_len < NFIT_SUBTABLE_HEADER_SIZE {
            break;
        }

        let end = match offset.checked_add(entry_len) {
            Some(v) => v,
            None => break,
        };
        if end > table_len {
            break;
        }

        let entry = &bytes[offset..end];

        match entry_type {
            NFIT_SUBTABLE_TYPE_SYSTEM_ADDRESS => {
                if entry_len < NFIT_SYSTEM_ADDRESS_MIN_LEN {
                    offset = end;
                    continue;
                }

                let Some(range_index) = read_u16_le(entry, 4) else {
                    offset = end;
                    continue;
                };
                let Some(address) = read_u64_le(entry, 32) else {
                    offset = end;
                    continue;
                };
                let Some(length) = read_u64_le(entry, 40) else {
                    offset = end;
                    continue;
                };
                let Some(range_guid) = entry.get(16..32) else {
                    offset = end;
                    continue;
                };
                let is_pmem = range_guid == NFIT_GUID_PERSISTENT_MEMORY.as_slice();

                spa_ranges.push(NfitSpaRange {
                    range_index,
                    address,
                    length,
                    is_pmem,
                });
            }
            NFIT_SUBTABLE_TYPE_MEMORY_MAP => {
                if entry_len < NFIT_MEMORY_MAP_LEN {
                    offset = end;
                    continue;
                }

                let Some(range_index) = read_u16_le(entry, 12) else {
                    offset = end;
                    continue;
                };
                let Some(region_size) = read_u64_le(entry, 16) else {
                    offset = end;
                    continue;
                };
                let Some(address) = read_u64_le(entry, 32) else {
                    offset = end;
                    continue;
                };
                let Some(flags) = read_u16_le(entry, 44) else {
                    offset = end;
                    continue;
                };

                memmap_ranges.push(NfitMemMapRange {
                    range_index,
                    address,
                    region_size,
                    flags,
                });
            }
            _ => {}
        }

        offset = end;
    }

    let mut selected_raw: Vec<(u64, u64)> = Vec::new();
    let pmem_spas: Vec<NfitSpaRange> = spa_ranges
        .iter()
        .copied()
        .filter(|spa| spa.is_pmem && spa.length > 0)
        .collect();

    if !pmem_spas.is_empty() {
        for spa in &pmem_spas {
            let has_map_failed = memmap_ranges.iter().any(|memdev| {
                memdev.range_index == spa.range_index
                    && (memdev.flags & ACPI_NFIT_MEM_MAP_FAILED) != 0
            });
            if has_map_failed {
                continue;
            }
            // QEMU NFIT 下 memdev.address 可能为 0（DPA/偏移语义），真正的系统物理地址由 SPA.address 给出。
            selected_raw.push((spa.address, spa.length));
        }
    } else if !memmap_ranges.is_empty() {
        for memdev in &memmap_ranges {
            if memdev.region_size == 0 {
                continue;
            }
            if (memdev.flags & ACPI_NFIT_MEM_MAP_FAILED) != 0 {
                continue;
            }
            selected_raw.push((memdev.address, memdev.region_size));
        }
    }

    let mut regions: Vec<(PhysAddr, usize)> = Vec::new();
    for (addr, len) in selected_raw {
        let start = match usize::try_from(addr) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let size = match usize::try_from(len) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let size = trim_to_block_aligned(size);
        if size == 0 {
            continue;
        }
        regions.push((PhysAddr::new(start), size));
    }

    let regions = normalize_regions(regions);
    regions
}

#[cfg(target_arch = "x86_64")]
fn collect_pmem_regions() -> Vec<(crate::mm::PhysAddr, usize)> {
    let regions = collect_pmem_regions_from_e820();
    if !regions.is_empty() {
        return regions;
    }

    collect_pmem_regions_from_nfit()
}

#[cfg(target_arch = "x86_64")]
#[unified_init(INITCALL_DEVICE)]
fn pmem_init() -> Result<(), SystemError> {
    let regions = collect_pmem_regions();
    if regions.is_empty() {
        return Ok(());
    }

    for (id, (start, size)) in regions.into_iter().enumerate() {
        let dev = PmemBlockDevice::new(start, size, id);
        let dev_name = dev.dev_name().to_string();
        let registered = dev.clone() as Arc<dyn BlockDevice>;
        block_dev_manager().register(registered)?;
        log::info!(
            "PMEM block device registered: /dev/{} start={:?} size={:#x}",
            dev_name,
            dev.region_start(),
            dev.usable_size()
        );
    }

    Ok(())
}

#[cfg(not(target_arch = "x86_64"))]
#[unified_init(INITCALL_DEVICE)]
fn pmem_init() -> Result<(), SystemError> {
    Ok(())
}
