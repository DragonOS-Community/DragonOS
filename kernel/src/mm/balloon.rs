//! Managed-memory accounting for pages owned by a virtio balloon.
//!
//! Physical ownership remains in the driver. An OOM-reclaimable balloon is
//! still managed memory, as in Linux; one which the guest cannot reclaim is
//! excluded from MemTotal/sysinfo, but never from the allocator's raw inventory.

use core::sync::atomic::{AtomicUsize, Ordering};

use crate::arch::mm::LockedFrameAllocator;

use super::allocator::page_frame::{FrameAllocator, PageFrameCount, PageFrameUsage};

static NON_RECLAIMABLE_PAGES: AtomicUsize = AtomicUsize::new(0);

/// Called only after taking exclusive frames from the allocator.
pub(crate) fn add_pages(pages: usize, reclaimable: bool) {
    if !reclaimable {
        NON_RECLAIMABLE_PAGES.fetch_add(pages, Ordering::AcqRel);
    }
}

/// Called before returning confirmed-deflated frames to the allocator.
pub(crate) fn remove_pages(pages: usize, reclaimable: bool) {
    if !reclaimable {
        let previous = NON_RECLAIMABLE_PAGES.fetch_sub(pages, Ordering::AcqRel);
        assert!(previous >= pages, "balloon accounting underflow");
    }
}

pub(crate) fn non_reclaimable_pages() -> usize {
    NON_RECLAIMABLE_PAGES.load(Ordering::Acquire)
}

/// Keep MemFree identical to the buddy snapshot. The driver publishes its
/// counter after allocation and removes it before freeing, but a reader can
/// span those operations; clamp against its older used snapshot accordingly.
pub(crate) fn managed_usage() -> PageFrameUsage {
    let usage = unsafe { LockedFrameAllocator.usage() };
    adjusted_usage(&usage, non_reclaimable_pages())
}

fn adjusted_usage(usage: &PageFrameUsage, excluded: usize) -> PageFrameUsage {
    let excluded = excluded.min(usage.used().data());
    PageFrameUsage::new(
        PageFrameCount::new(usage.used().data() - excluded),
        PageFrameCount::new(usage.total().data() - excluded),
    )
}

#[cfg(feature = "mm_selftest")]
pub(crate) fn accounting_selftest() -> bool {
    let usage = PageFrameUsage::new(PageFrameCount::new(400), PageFrameCount::new(1000));
    [0, 100, 400, 401, usize::MAX].into_iter().all(|excluded| {
        let adjusted = adjusted_usage(&usage, excluded);
        adjusted.free() == usage.free()
            && adjusted.used().data() <= adjusted.total().data()
            && adjusted.total().data() == 1000 - excluded.min(400)
    })
}
