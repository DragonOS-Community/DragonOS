//! Simplified `mmu_gather` for DragonOS (cf. Linux 6.6 `include/asm-generic/tlb.h`)
//!
//! Enforces the ordering: shootdown first, then free physical pages / page-table pages (INV-3):
//!
//! 1) Page table entries are cleared by PageMapper.
//! 2) Arc references to physical pages are stashed in `pending_pages` instead of being dropped immediately.
//! 3) `flush_mmu_tlbonly()` performs remote + local TLB invalidation via `AddressSpace::flush_tlb_range` (synchronous ack).
//! 4) `flush_mmu_free()` finally drops pending_pages, triggering `deallocate_page_frames`.
//!
//! This guarantees that no matter how many other CPUs share the mm, they cannot hit a freed
//! physical page through a stale TLB entry.

use alloc::{sync::Arc, vec::Vec};

use crate::{
    arch::MMArch,
    mm::{
        page::{page_manager_lock, Page},
        tlb::TLB_FLUSH_ALL,
        ucontext::AddressSpace,
        MemoryManagementArch, PhysAddr, VirtAddr,
    },
};

/// Batch processor for a single mmu modification session.
///
/// Usage:
///
/// ```ignore
/// let mut tlb = MmuGather::gather(mm, start, end);
/// // ... call PageMapper to unmap, pass the returned PhysAddr to tlb.remove_page(paddr) ...
/// tlb.finish(); // flush TLB first, then free pages
/// ```
pub struct MmuGather<'mm> {
    /// Target mm for shootdown.
    ///
    /// `None` means "no shootdown needed" — used on the teardown path (`Drop for InnerAddressSpace`)
    /// where the `Arc<AddressSpace>` itself is being dropped, so its strong-count has already hit 0,
    /// `Weak::upgrade` can no longer obtain the Arc, and `active_cpus` has already been cleared by
    /// all process-exit paths. In this case there is literally no CPU holding a TLB entry for this mm,
    /// so `flush_mmu_tlbonly` becomes a no-op while the page-free bookkeeping still runs.
    mm: Option<&'mm Arc<AddressSpace>>,
    /// Range start (to be accumulated)
    start: VirtAddr,
    /// Range end (to be accumulated, exclusive)
    end: VirtAddr,
    /// Whether to perform a full-mm flush (common for large munmap)
    fullmm: bool,
    /// Whether page-table pages were freed (Linux freed_tables)
    freed_tables: bool,
    /// Accumulated physical pages pending release (dropped after flush_mmu_tlbonly)
    pending_pages: Vec<Arc<Page>>,
    /// Stride shift (typically PAGE_SHIFT)
    stride_shift: u8,
    /// Whether finish() has been called, preventing double free
    finished: bool,
}

impl<'mm> MmuGather<'mm> {
    /// Start a gather. Initial range is empty; expand via `accumulate_range` later.
    pub fn gather(mm: &'mm Arc<AddressSpace>) -> Self {
        Self {
            mm: Some(mm),
            start: VirtAddr::new(usize::MAX),
            end: VirtAddr::new(0),
            fullmm: false,
            freed_tables: false,
            pending_pages: Vec::new(),
            stride_shift: MMArch::PAGE_SHIFT as u8,
            finished: false,
        }
    }

    /// Gather without a shootdown target. Used only from the mm-teardown path
    /// (`InnerAddressSpace::drop`), where the `Arc<AddressSpace>` is already being dropped
    /// and `active_cpus` is guaranteed empty.
    ///
    /// `flush_mmu_tlbonly` is a no-op on this variant; `finish()` still drops stashed pages.
    pub fn gather_teardown() -> Self {
        Self {
            mm: None,
            start: VirtAddr::new(usize::MAX),
            end: VirtAddr::new(0),
            fullmm: false,
            freed_tables: false,
            pending_pages: Vec::new(),
            stride_shift: MMArch::PAGE_SHIFT as u8,
            finished: false,
        }
    }

    /// Mark the entire mm as needing a flush (full-segment munmap or exit path).
    pub fn set_fullmm(&mut self) {
        self.fullmm = true;
    }

    /// Mark that this gather also freed page-table pages.
    pub fn note_pt_table_freed(&mut self) {
        self.freed_tables = true;
    }

    /// Merge a `[vaddr, vaddr + (1 << stride_shift))` interval into the flush range.
    pub fn accumulate_range(&mut self, vaddr: VirtAddr) {
        let stride = 1usize << self.stride_shift;
        let page_end = VirtAddr::new(vaddr.data().saturating_add(stride));

        if vaddr < self.start {
            self.start = vaddr;
        }
        if page_end > self.end {
            self.end = page_end;
        }
    }

    /// Add a physical page pending release to the pending list; actual release happens after TLB flush.
    ///
    /// `paddr` points to a physical page already unmapped from the page table. The caller must ensure
    /// that the anon_vma reverse mapping for this page has been cleaned up (e.g. by obtaining the
    /// `Arc<Page>` via `PageManagerRef::remove_page` before passing it in).
    pub fn stash_page(&mut self, page: Arc<Page>) {
        self.pending_pages.push(page);
    }

    /// Convenience: remove the `Arc<Page>` for `paddr` from the global page_manager and stash it.
    ///
    /// If `paddr` does not exist in the page_manager (e.g. a file page no longer in cache), this is a no-op.
    pub fn stash_paddr(&mut self, paddr: PhysAddr) {
        let mut guard = page_manager_lock();
        if let Some(p) = guard.remove_page(&paddr) {
            self.pending_pages.push(p);
        }
    }

    /// Perform TLB flush only (without freeing pending_pages).
    ///
    /// After this call, `start/end/freed_tables` are reset, allowing a second round of accumulation
    /// within the same gather.
    pub fn flush_mmu_tlbonly(&mut self) {
        if let Some(mm) = self.mm {
            if self.fullmm || self.start == VirtAddr::new(usize::MAX) {
                mm.flush_tlb_range(
                    VirtAddr::new(0),
                    TLB_FLUSH_ALL,
                    self.stride_shift,
                    self.freed_tables || self.fullmm,
                );
            } else if self.start < self.end {
                mm.flush_tlb_range(self.start, self.end, self.stride_shift, self.freed_tables);
            }
        }
        // else: teardown path -- no CPU holds a TLB entry for this mm anymore, skip shootdown.

        self.start = VirtAddr::new(usize::MAX);
        self.end = VirtAddr::new(0);
        self.freed_tables = false;
        self.fullmm = false;
    }

    /// Free pending_pages (triggers deallocate_page_frames).
    ///
    /// The caller must guarantee that TLB shootdown has already completed before calling this.
    pub fn flush_mmu_free(&mut self) {
        // Drop all Arc<Page>, letting InnerPage::drop reach deallocate_page_frames
        self.pending_pages.clear();
    }

    /// Finish the gather: flush TLB first, then free pages.
    pub fn finish(mut self) {
        self.flush_mmu_tlbonly();
        self.flush_mmu_free();
        self.finished = true;
    }
}

impl Drop for MmuGather<'_> {
    fn drop(&mut self) {
        if !self.finished {
            // Normal path must call finish() explicitly; this is a fallback to avoid
            // missing a flush during panic unwind.
            self.flush_mmu_tlbonly();
            self.flush_mmu_free();
        }
    }
}
