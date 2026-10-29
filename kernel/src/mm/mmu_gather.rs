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

use alloc::sync::Arc;

use crate::{
    arch::MMArch,
    mm::{
        page::{page_manager_lock, DetachedUserTables, Page},
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
enum GatherTarget<'mm> {
    AddressSpace(&'mm Arc<AddressSpace>),
    /// Bootstrap/shared kernel mappings may be loaded without mm tracking.
    AllOnline,
    /// An unpublished/inactive mm cannot have stale translations.
    Inactive,
}

pub(crate) const PAGE_BATCH: usize = 32;

pub struct MmuGather<'mm> {
    /// Translation holders to invalidate before physical retirement.
    ///
    /// `Inactive` means "no shootdown needed" — used on the teardown path (`Drop for InnerAddressSpace`)
    /// where the `Arc<AddressSpace>` itself is being dropped, so its strong-count has already hit 0,
    /// `Weak::upgrade` can no longer obtain the Arc, and `active_cpus` has already been cleared by
    /// all process-exit paths. In this case there is literally no CPU holding a TLB entry for this mm,
    /// so `flush_mmu_tlbonly` becomes a no-op while the page-free bookkeeping still runs.
    target: GatherTarget<'mm>,
    /// Range start (to be accumulated)
    start: VirtAddr,
    /// Range end (to be accumulated, exclusive)
    end: VirtAddr,
    /// Whether to perform a full-mm flush (common for large munmap)
    fullmm: bool,
    /// Whether page-table pages were freed (Linux freed_tables)
    freed_tables: bool,
    /// Accumulated physical pages pending release (dropped after flush_mmu_tlbonly)
    pending_pages: [Option<Arc<Page>>; PAGE_BATCH],
    pending_count: usize,
    pending_tables: DetachedUserTables,
    /// Stride shift (typically PAGE_SHIFT)
    stride_shift: u8,
    /// Whether finish() has been called, preventing double free
    finished: bool,
    #[cfg(all(feature = "mm_selftest", target_arch = "x86_64"))]
    observer: Option<Arc<crate::mm::tlb::selftest::Observer>>,
}

impl<'mm> MmuGather<'mm> {
    /// Start a gather. Initial range is empty; expand via `accumulate_range` later.
    pub fn gather(mm: &'mm Arc<AddressSpace>) -> Self {
        Self {
            target: GatherTarget::AddressSpace(mm),
            start: VirtAddr::new(usize::MAX),
            end: VirtAddr::new(0),
            fullmm: false,
            freed_tables: false,
            pending_pages: core::array::from_fn(|_| None),
            pending_count: 0,
            pending_tables: DetachedUserTables::new(),
            stride_shift: MMArch::PAGE_SHIFT as u8,
            finished: false,
            #[cfg(all(feature = "mm_selftest", target_arch = "x86_64"))]
            observer: crate::mm::tlb::selftest::observer_for(mm),
        }
    }

    /// Gather without a shootdown target. Used only from the mm-teardown path
    /// (`InnerAddressSpace::drop`), where the `Arc<AddressSpace>` is already being dropped
    /// and `active_cpus` is guaranteed empty.
    ///
    /// `flush_mmu_tlbonly` is a no-op on this variant; `finish()` still drops stashed pages.
    pub fn gather_teardown() -> Self {
        Self {
            target: GatherTarget::Inactive,
            start: VirtAddr::new(usize::MAX),
            end: VirtAddr::new(0),
            fullmm: false,
            freed_tables: false,
            pending_pages: core::array::from_fn(|_| None),
            pending_count: 0,
            pending_tables: DetachedUserTables::new(),
            stride_shift: MMArch::PAGE_SHIFT as u8,
            finished: false,
            #[cfg(all(feature = "mm_selftest", target_arch = "x86_64"))]
            observer: None,
        }
    }

    /// Bootstrap mappings are installed by direct CR3 writes before normal
    /// active-mm tracking. Retire their tables only after every online CPU
    /// acknowledges the shootdown. The caller must finish outside MM/IRQ locks.
    pub(crate) fn gather_all_online() -> Self {
        let mut gather = Self::gather_teardown();
        gather.target = GatherTarget::AllOnline;
        gather
    }

    /// Mark the entire mm as needing a flush (full-segment munmap or exit path).
    pub fn set_fullmm(&mut self) {
        self.fullmm = true;
    }

    /// Mark that this gather also freed page-table pages.
    pub fn note_pt_table_freed(&mut self) {
        self.freed_tables = true;
    }

    pub(crate) fn stash_user_tables(&mut self, tables: DetachedUserTables) {
        if !tables.is_empty() {
            self.freed_tables = true;
        }
        self.pending_tables.append(tables);
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

    /// Retain a revoked managed PTE's Page through this transaction's barrier.
    ///
    /// Callers have removed this mapping's rmap. They must stash every revoked
    /// managed page, including one still mapped by another MM: the last rmap
    /// remover's barrier cannot stand in for this MM's pending shootdown.
    /// Eligible registry ownership may already be detached by the Page layer;
    /// remaining mappings, cache/backing owners and access pins stay valid.
    /// May flush a full batch. No Page, page-manager or reclaimer guard may
    /// remain held: the barrier waits for remote CPUs before retiring owners.
    pub fn stash_page(&mut self, page: Arc<Page>) {
        self.pending_pages[self.pending_count] = Some(page);
        self.pending_count += 1;
        if self.pending_count == PAGE_BATCH {
            self.flush_mmu_tlbonly();
            self.flush_mmu_free();
        }
    }

    /// Convenience: remove the `Arc<Page>` for `paddr` from the global page_manager and stash it.
    ///
    /// If `paddr` does not exist in the page_manager (e.g. a file page no longer in cache), this is a no-op.
    pub fn stash_paddr(&mut self, paddr: PhysAddr) {
        let page = { page_manager_lock().remove_page(&paddr) };
        if let Some(page) = page {
            self.stash_page(page);
        }
    }

    /// Perform TLB flush only (without freeing pending_pages).
    ///
    /// After this call, `start/end/freed_tables` are reset, allowing a second round of accumulation
    /// within the same gather.
    pub fn flush_mmu_tlbonly(&mut self) {
        #[cfg(all(feature = "mm_selftest", target_arch = "x86_64"))]
        if let (Some(observer), GatherTarget::AddressSpace(mm)) = (&self.observer, &self.target) {
            // The caller owns the mm mutation lock. A borrowed, read-only
            // mapper verifies the real leaf without recursively locking mm.
            let mapper = unsafe {
                crate::arch::mm::PageMapper::new(
                    crate::mm::PageTableKind::User,
                    mm.table_paddr(),
                    crate::arch::mm::LockedFrameAllocator,
                )
            };
            observer.cleared(mapper.translate(observer.start).is_none());
        }
        if let GatherTarget::AddressSpace(mm) = self.target {
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
        } else if matches!(self.target, GatherTarget::AllOnline) {
            let (start, end) = if self.fullmm || self.start == VirtAddr::new(usize::MAX) {
                (VirtAddr::new(0), TLB_FLUSH_ALL)
            } else {
                (self.start, self.end)
            };
            crate::mm::tlb::flush_tlb_kernel_range(start, end, self.freed_tables || self.fullmm);
        }
        // Inactive: no CPU holds a translation, so retirement needs no shootdown.

        self.start = VirtAddr::new(usize::MAX);
        self.end = VirtAddr::new(0);
        self.freed_tables = false;
        self.fullmm = false;
    }

    /// Free pending_pages (triggers deallocate_page_frames).
    ///
    /// The caller must guarantee that TLB shootdown has already completed before calling this.
    pub fn flush_mmu_free(&mut self) {
        #[cfg(all(feature = "mm_selftest", target_arch = "x86_64"))]
        if let Some(observer) = self.observer.clone() {
            // This check is at the real retirement boundary. A skipped flush
            // is detected before rescue, and rescue is never positive evidence.
            if !observer.before_free() {
                self.flush_mmu_tlbonly();
            }
            assert!(observer.before_free(), "unsafe selftest page retirement");
            for slot in &mut self.pending_pages[..self.pending_count] {
                let Some(page) = slot.take() else { continue };
                if page.phys_address().data()
                    == observer.frame.load(core::sync::atomic::Ordering::Acquire)
                {
                    match Arc::try_unwrap(page) {
                        Ok(page) => {
                            let owns_frame = page.read().individually_owns_frame();
                            if !owns_frame {
                                observer.fail();
                            }
                            assert!(observer.before_free());
                            drop(page);
                            if owns_frame {
                                observer.frame_freed();
                            }
                        }
                        Err(page) => {
                            observer.fail();
                            drop(page);
                        }
                    }
                } else {
                    // The observer owns one real frame; unrelated members of
                    // a multi-page batch follow ordinary retirement.
                    drop(page);
                }
            }
            self.pending_count = 0;
            let had_tables = !self.pending_tables.is_empty();
            assert!(observer.before_free());
            unsafe { self.pending_tables.free_after_flush() };
            if had_tables {
                observer.tables_freed();
            }
            return;
        }
        // Drop all Arc<Page>, letting InnerPage::drop reach deallocate_page_frames
        for slot in &mut self.pending_pages[..self.pending_count] {
            drop(slot.take());
        }
        self.pending_count = 0;
        unsafe { self.pending_tables.free_after_flush() };
    }

    /// Finish the gather: flush TLB first, then free pages.
    pub fn finish(mut self) {
        #[cfg(all(feature = "mm_selftest", target_arch = "x86_64"))]
        let skip = self
            .observer
            .as_ref()
            .is_some_and(|observer| observer.skip_flush());
        #[cfg(not(all(feature = "mm_selftest", target_arch = "x86_64")))]
        let skip = false;
        if !skip {
            self.flush_mmu_tlbonly();
        }
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
