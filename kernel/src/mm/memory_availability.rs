//! System-wide estimate of memory available for ordinary userspace allocations.

use crate::arch::mm::LockedFrameAllocator;

use super::{
    allocator::{
        buddy::UserAllocationSnapshot,
        page_frame::{PageFrameCount, PageFrameUsage},
    },
    balloon,
};

pub(crate) fn managed_usage_with_available() -> (PageFrameUsage, PageFrameCount) {
    #[cfg(any(target_arch = "x86_64", target_arch = "riscv64"))]
    let allocator = LockedFrameAllocator::with_buddy(|buddy| buddy.user_allocation_snapshot())
        .expect("physical allocator not initialized");
    #[cfg(target_arch = "loongarch64")]
    let allocator = {
        use super::allocator::page_frame::FrameAllocator;
        UserAllocationSnapshot {
            usage: unsafe { LockedFrameAllocator.usage() },
            low_watermark_pages: 0,
            reserve_pages: 0,
        }
    };
    let UserAllocationSnapshot {
        usage,
        low_watermark_pages,
        reserve_pages,
    } = allocator;
    let usage = balloon::adjust_managed_usage(&usage);
    let file_lru_pages = crate::cgroup::memory::global_file_lru_pages();
    let available = estimate(&usage, file_lru_pages, low_watermark_pages, reserve_pages);
    (usage, PageFrameCount::new(available))
}

fn estimate(
    usage: &PageFrameUsage,
    file_lru_pages: u64,
    low_watermark_pages: usize,
    reserve_pages: usize,
) -> usize {
    // Allocator and LRU counters are sampled separately. Keep cache credit
    // within the used inventory even if the samples straddle a reclamation.
    let cache = file_lru_pages.min(usage.used().data() as u64) as usize;
    // As in Linux si_mem_available(), retain up to half the file LRU, capped
    // by the low watermarks. There is no reclaimable slab inventory yet.
    let cache_credit = cache - (cache / 2).min(low_watermark_pages);
    (usage.free().data() + cache_credit).saturating_sub(reserve_pages)
}

#[cfg(feature = "mm_selftest")]
pub(super) fn selftest() -> bool {
    let usage = |free: usize| {
        PageFrameUsage::new(PageFrameCount::new(100 - free), PageFrameCount::new(100))
    };
    estimate(&usage(20), 0, 10, 20) == 0
        && estimate(&usage(10), 0, 10, 20) == 0
        && estimate(&usage(10), 14, 10, 20) == 0
        && estimate(&usage(10), 24, 10, 20) == 4
        && estimate(&usage(80), u64::MAX, 4, 12) == 84
}
