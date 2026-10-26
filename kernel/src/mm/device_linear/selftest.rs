//! Structural visibility only: synthetic PFNs are never dereferenced or DMA targets.
use super::*;
use crate::{
    arch::mm::{LockedFrameAllocator, PageMapper},
    mm::{vmalloc::VmAllocation, PageTableKind},
};
use core::alloc::Layout;

fn range_guards() -> bool {
    let page = MMArch::PAGE_SIZE;
    for (physical, length) in [(0, 0), (1, page), (0, page - 1)] {
        if validate_range(PhysAddr::new(physical), length) != Err(SystemError::EINVAL) {
            return false;
        }
    }
    for (start, length) in [
        (MMArch::MMIO_BASE.data(), MMArch::MMIO_SIZE),
        (MMArch::FIXMAP_START_VADDR.data(), MMArch::FIXMAP_SIZE),
    ] {
        let Some(physical) = start.checked_sub(MMArch::PHYS_OFFSET + page) else {
            return false;
        };
        if validate_range(PhysAddr::new(physical), length + 2 * page) != Err(SystemError::ERANGE) {
            return false;
        }
        // Adjacent half-open ranges are legal; do not reject a whole PGD slot.
        for adjacent in [start - page, start + length] {
            if validate_range(PhysAddr::new(adjacent - MMArch::PHYS_OFFSET), page).is_err() {
                return false;
            }
        }
    }
    let Ok(allocation) = VmAllocation::try_new_zeroed(Layout::from_size_align(page, page).unwrap())
    else {
        return false;
    };
    let physical = allocation.address().data() - MMArch::PHYS_OFFSET;
    if validate_range(PhysAddr::new(physical), page) != Err(SystemError::ERANGE) {
        return false;
    }
    // Span the entire reserved window with both endpoints outside it. Derive
    // its geometry from a real allocation and capacity, not a copied BASE.
    let capacity = crate::mm::vmalloc::capacity_bytes() as usize;
    let start = allocation.address().data() - capacity;
    let end = allocation.address().data() + capacity;
    if crate::mm::vmalloc::contains(VirtAddr::new(start))
        || crate::mm::vmalloc::contains(VirtAddr::new(end - 1))
        || validate_range(PhysAddr::new(start - MMArch::PHYS_OFFSET), end - start)
            != Err(SystemError::ERANGE)
    {
        return false;
    }
    validate_range(PhysAddr::new(usize::MAX & !(page - 1)), 2 * page) == Err(SystemError::EOVERFLOW)
}

pub(crate) fn run() -> bool {
    if !range_guards() {
        return false;
    }
    // These are real architecture-created user roots, born before the new leaf.
    let Ok(first) = MMArch::setup_new_usermapper(None) else {
        return false;
    };
    let Ok(second) = MMArch::setup_new_usermapper(None) else {
        return false;
    };
    let initial = unsafe {
        PageMapper::new(
            PageTableKind::Kernel,
            MMArch::initial_page_table(),
            LockedFrameAllocator,
        )
    };
    let candidate = {
        let kernel = KernelMapper::lock();
        let top_shift = MMArch::PAGE_SHIFT + (MMArch::PAGE_LEVELS - 1) * MMArch::PAGE_ENTRY_SHIFT;
        (1..MMArch::PAGE_ENTRY_NUM - MMArch::PAGE_KERNEL_INDEX).find_map(|slot| {
            let physical = slot.checked_shl(top_shift as u32)?;
            let (address, _) = validate_range(PhysAddr::new(physical), MMArch::PAGE_SIZE).ok()?;
            let table = kernel.get_table(address, MMArch::PAGE_LEVELS - 2)?;
            let empty = (0..MMArch::PAGE_ENTRY_NUM)
                .all(|index| unsafe { table.entry(index) }.is_some_and(|entry| entry.empty()));
            empty.then_some((PhysAddr::new(physical), address))
        })
    };
    let Some((physical, address)) = candidate else {
        return false;
    };
    let parent = {
        let kernel = KernelMapper::lock();
        let Some(entry) = kernel.get_entry(address, MMArch::PAGE_LEVELS - 1) else {
            return false;
        };
        if !entry.present() || entry.flags().has_user() {
            return false;
        }
        let Ok(parent) = entry.address() else {
            return false;
        };
        for mapper in [&first.utable, &second.utable, &initial] {
            if mapper
                .get_entry(address, MMArch::PAGE_LEVELS - 1)
                .and_then(|entry| entry.address().ok())
                != Some(parent)
                || mapper.translate(address).is_some()
            {
                return false;
            }
        }
        parent
    };
    // Repeat at the same VA after real teardown, reusing the permanent parent.
    for _ in 0..2 {
        let flags = EntryFlags::new()
            .set_user(false)
            .set_write(true)
            .set_execute(false)
            .set_page_global(false);
        // No access through address or physical follows: only page-table reads.
        let Ok(mapping) = DeviceLinearMapping::new(physical, MMArch::PAGE_SIZE, flags) else {
            return false;
        };
        let installed = {
            let kernel = KernelMapper::lock();
            let stable = !(MMArch::ENTRY_FLAG_ACCESSED | MMArch::ENTRY_FLAG_DIRTY);
            [&first.utable, &second.utable, &initial, kernel.as_ref()]
                .iter()
                .all(|mapper| {
                    mapper.translate(address).is_some_and(|(frame, actual)| {
                        frame == physical && actual.data() & stable == flags.data() & stable
                    }) && mapper
                        .get_entry(address, MMArch::PAGE_LEVELS - 1)
                        .and_then(|entry| entry.address().ok())
                        == Some(parent)
                })
        };
        // Real teardown performs the kernel shootdown and frees only owned tables.
        drop(mapping);
        let removed = {
            let kernel = KernelMapper::lock();
            [&first.utable, &second.utable, &initial, kernel.as_ref()]
                .iter()
                .all(|mapper| {
                    mapper.translate(address).is_none()
                        && mapper
                            .get_entry(address, MMArch::PAGE_LEVELS - 1)
                            .and_then(|entry| entry.address().ok())
                            == Some(parent)
                })
        };
        if !installed || !removed {
            return false;
        }
    }
    true
}
