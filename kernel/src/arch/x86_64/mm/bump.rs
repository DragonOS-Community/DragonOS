use crate::mm::{
    allocator::bump::BumpAllocator,
    memblock::{mem_block_manager, MemoryAreaAttr, INITIAL_MEMORY_REGIONS_NUM},
    MemoryManagementArch, PhysAddr, PhysMemoryArea, VirtAddr,
};
use system_error::SystemError;

use super::{X86_64MMBootstrapInfo, BOOTSTRAP_MM_INFO};

// Written in place by the BSP before blanket reservation, then read-only.
// Do not put this array in the Copy bootstrap-info value or on the boot stack.
static mut RECLAIMABLE_PREFIX: [PhysMemoryArea; INITIAL_MEMORY_REGIONS_NUM] =
    [PhysMemoryArea::DEFAULT; INITIAL_MEMORY_REGIONS_NUM];
static mut PREFIX_COUNT: usize = 0;
static mut PREFIX_READY: bool = false;

/// Preserve only original available RAM inside the existing bootstrap-padding
/// window. Blanket reservation subsequently destroys the distinction between
/// this RAM and firmware/module reservations; it must never create authority
/// to reclaim those reservations.
pub(super) unsafe fn snapshot_reclaimable_prefix() -> Result<(), SystemError> {
    if PREFIX_READY {
        return Err(SystemError::EEXIST);
    }
    let info: X86_64MMBootstrapInfo = BOOTSTRAP_MM_INFO.ok_or(SystemError::EINVAL)?;
    let kernel_start = super::MMArch::virt_2_phys(VirtAddr::new(info.kernel_code_start))
        .ok_or(SystemError::EINVAL)?
        .data();
    let mask = super::MMArch::PAGE_SIZE - 1;
    let start = info
        .kernel_load_base_paddr
        .checked_add(16384)
        .map(|address| address.max(0x200000))
        .and_then(|address| address.checked_add(mask))
        .ok_or(SystemError::EINVAL)?
        & !mask;
    let end = kernel_start.checked_sub(16384).ok_or(SystemError::EINVAL)? & !mask;
    let mut count = 0;
    if start < end {
        for area in mem_block_manager().to_iter_available() {
            if area
                .flags
                .intersects(MemoryAreaAttr::NOMAP | MemoryAreaAttr::DRIVER_MANAGED)
            {
                return Err(SystemError::EINVAL);
            }
            let area_start = area
                .base
                .data()
                .checked_add(mask)
                .ok_or(SystemError::EINVAL)?
                & !mask;
            let area_end = area
                .base
                .data()
                .checked_add(area.size)
                .ok_or(SystemError::EINVAL)?
                & !mask;
            let base = start.max(area_start);
            let limit = end.min(area_end);
            if base >= limit {
                continue;
            }
            if count == INITIAL_MEMORY_REGIONS_NUM {
                return Err(SystemError::ENOMEM);
            }
            core::ptr::addr_of_mut!(RECLAIMABLE_PREFIX)
                .cast::<PhysMemoryArea>()
                .add(count)
                .write(PhysMemoryArea::new(
                    PhysAddr::new(base),
                    limit - base,
                    MemoryAreaAttr::empty(),
                ));
            count += 1;
        }
    }
    PREFIX_COUNT = count;
    PREFIX_READY = true;
    Ok(())
}

impl<MMA: MemoryManagementArch> BumpAllocator<MMA> {
    pub unsafe fn arch_remain_areas(
        ret_areas: &mut [PhysMemoryArea],
        res_count: usize,
    ) -> Option<usize> {
        if !PREFIX_READY {
            return None;
        }
        let total = res_count.checked_add(PREFIX_COUNT)?;
        if total > ret_areas.len() {
            return None;
        }
        for index in 0..PREFIX_COUNT {
            ret_areas[res_count + index] = core::ptr::addr_of!(RECLAIMABLE_PREFIX)
                .cast::<PhysMemoryArea>()
                .add(index)
                .read();
        }
        Some(total)
    }
}
