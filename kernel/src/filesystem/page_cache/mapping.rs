use super::{
    page_align_up, page_manager_lock, page_reclaimer_lock, AddressSpace, Arc, FaultRetryWait,
    HashMap, LockedVMA, MMArch, MemoryManagementArch, MmuGather, Ordering, PageCache,
    PageCacheManager, PageState, RwSemReadGuard, RwSemWriteGuard, SystemError, Vec, Weak,
};

#[derive(Debug, Default)]
pub(super) struct FileVmaIndex {
    vmas: HashMap<usize, Weak<LockedVMA>>,
}

impl FileVmaIndex {
    fn register(&mut self, vma: &Arc<LockedVMA>) {
        self.vmas.insert(vma.id(), Arc::downgrade(vma));
    }

    fn unregister(&mut self, vma_id: usize) {
        self.vmas.remove(&vma_id);
    }

    fn collect_all(&mut self) -> Vec<Arc<LockedVMA>> {
        let mut result = Vec::new();
        self.vmas.retain(|_, weak| {
            if let Some(vma) = weak.upgrade() {
                result.push(vma);
                true
            } else {
                false
            }
        });
        result
    }
}

struct MmFileRangeGroup {
    mm: Arc<AddressSpace>,
    ranges: Vec<(Arc<LockedVMA>, crate::mm::VirtRegion)>,
}

impl MmFileRangeGroup {
    fn new(mm: Arc<AddressSpace>) -> Self {
        Self {
            mm,
            ranges: Vec::new(),
        }
    }
}

struct MmFilePageGroup {
    mm: Arc<AddressSpace>,
    items: Vec<(Arc<LockedVMA>, crate::mm::VirtAddr)>,
}

impl MmFilePageGroup {
    fn new(mm: Arc<AddressSpace>) -> Self {
        Self {
            mm,
            items: Vec::new(),
        }
    }
}

/// Policy for zapping page-cache backed file mappings.
///
/// This mirrors Linux's `unmap_mapping_pages(..., even_cows)`: cache invalidation
/// must preserve private COW data, while truncate must also drop COWed private
/// PTEs so future access faults against the new file size.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UnmapMappingMode {
    CacheOnly,
    EvenCow,
}

struct PageCacheInvalidateRetryWait {
    cache: Arc<PageCache>,
}

impl core::fmt::Debug for PageCacheInvalidateRetryWait {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PageCacheInvalidateRetryWait").finish()
    }
}

impl FaultRetryWait for PageCacheInvalidateRetryWait {
    fn wait(&self) -> Result<(), SystemError> {
        let _invalidate = self.cache.invalidate_read();
        Ok(())
    }
}

/// Result of the nonblocking invalidation decision made before a file-backed
/// write fault enters filesystem code.
///
/// `Retry` is deliberately an owned wait token rather than a boolean. The
/// caller must return `VM_FAULT_RETRY`, release its `AddressSpace::write()`
/// guard in the outer fault loop, and only then call `wait()`. Keeping that
/// decision with PageCache makes the writer-preference rule and its retry
/// predicate single-sourced for both shared and private file faults.
#[must_use = "a file-fault invalidation decision must be held or returned as VM_FAULT_RETRY"]
pub(crate) enum PageCacheFaultInvalidateRead<'a> {
    Acquired(RwSemReadGuard<'a, ()>),
    Retry(Arc<dyn FaultRetryWait>),
}

impl PageCacheManager {
    pub fn resize(&self, len: usize) -> Result<(), SystemError> {
        let cache = self.upgrade()?;
        cache.truncate(len)
    }
}

impl PageCacheManager {
    pub fn invalidate_range(
        &self,
        start_index: usize,
        end_index: usize,
    ) -> Result<usize, SystemError> {
        Ok(self
            .upgrade()?
            .evict_clean_pages_for_invalidate(Some((start_index, end_index))))
    }

    pub fn discard_clean_range(
        &self,
        start_index: usize,
        end_index: usize,
    ) -> Result<usize, SystemError> {
        self.discard_clean_range_inner(start_index, end_index, true)
    }

    /// Discard immediately reclaimable clean pages without waiting for I/O.
    ///
    /// This is used while acknowledging a FUSE notification: waiting for a
    /// Loading page there can deadlock the daemon that must complete that load.
    pub(crate) fn discard_clean_range_nowait(
        &self,
        start_index: usize,
        end_index: usize,
    ) -> Result<usize, SystemError> {
        self.discard_clean_range_inner(start_index, end_index, false)
    }

    fn discard_clean_range_inner(
        &self,
        start_index: usize,
        end_index: usize,
        wait_loading: bool,
    ) -> Result<usize, SystemError> {
        let cache = self.upgrade()?;
        if cache.is_shmem() {
            return Ok(0);
        }
        let indices = cache.clean_evict_indices(Some((start_index, end_index)));

        let mut discarded = 0;
        for page_index in indices {
            if let Some(page) = cache.remove_clean_page_candidate(page_index, wait_loading) {
                let paddr = page.phys_address();
                let can_remove_from_manager = page.read().can_deallocate();
                let _ = page_reclaimer_lock().remove_page(&paddr);
                if can_remove_from_manager {
                    page_manager_lock().remove_page(&paddr);
                }
                discarded += 1;
            }
        }

        Ok(discarded)
    }

    pub fn invalidate_all_clean(&self) -> Result<usize, SystemError> {
        let cache = self.upgrade()?;
        if cache.is_shmem() {
            return Ok(0);
        }
        let dropped = cache.evict_clean_pages_for_invalidate(None);
        Ok(dropped)
    }

    pub(crate) fn discard_clean_page(&self, page_index: usize) -> Result<(), SystemError> {
        let cache = self.upgrade()?;
        if cache.is_shmem() {
            return Ok(());
        }
        if let Some(page) = cache.remove_clean_page_candidate(page_index, true) {
            cache.discard_unlinked_page(&page);
        }
        Ok(())
    }
}

impl PageCache {
    pub fn i_mmap_read(&self) -> RwSemReadGuard<'_, ()> {
        self.i_mmap_rwsem.read()
    }

    pub fn i_mmap_write(&self) -> RwSemWriteGuard<'_, ()> {
        self.i_mmap_rwsem.write()
    }

    pub fn invalidate_read(&self) -> RwSemReadGuard<'_, ()> {
        self.invalidate_lock.read()
    }

    pub fn try_invalidate_read(&self) -> Option<RwSemReadGuard<'_, ()>> {
        self.invalidate_lock.try_read()
    }

    /// Acquire the file-fault invalidation read side without sleeping, or
    /// return the precise retry predicate that the outer fault loop may wait
    /// on after it has released `AddressSpace::write()`.
    ///
    /// This is the sole admission decision for shared writable and private
    /// COW file faults. Do not replace it with `invalidate_read()` in a fault
    /// handler: a queued buffered writer may otherwise turn a writeback
    /// snapshot's `invalidate_read() -> AddressSpace::read()` edge into an
    /// MM/writeback/buffered-write cycle.
    pub(crate) fn file_fault_invalidate_read(self: &Arc<Self>) -> PageCacheFaultInvalidateRead<'_> {
        match self.try_invalidate_read() {
            Some(guard) => PageCacheFaultInvalidateRead::Acquired(guard),
            None => PageCacheFaultInvalidateRead::Retry(self.invalidate_retry_wait()),
        }
    }

    /// Build a fault retry token after a nonblocking invalidation-read attempt
    /// failed. The caller must return to the architecture fault loop, which
    /// drops `AddressSpace::write()` before invoking the token's blocking
    /// `wait()` method.
    pub fn invalidate_retry_wait(self: &Arc<Self>) -> Arc<dyn FaultRetryWait> {
        Arc::new(PageCacheInvalidateRetryWait {
            cache: self.clone(),
        })
    }

    pub fn invalidate_write(&self) -> RwSemWriteGuard<'_, ()> {
        self.invalidate_lock.write()
    }

    fn note_file_vma_mutation(&self) {
        self.file_vma_seq.fetch_add(1, Ordering::AcqRel);
    }

    pub fn file_vma_seq(&self) -> u64 {
        self.file_vma_seq.load(Ordering::Acquire)
    }

    pub fn register_file_vma(&self, vma: &Arc<LockedVMA>) {
        let _guard = self.i_mmap_write();
        self.file_vmas.lock_irqsave().register(vma);
        self.note_file_vma_mutation();
    }

    pub fn unregister_file_vma(&self, vma_id: usize) {
        let _guard = self.i_mmap_write();
        self.file_vmas.lock_irqsave().unregister(vma_id);
        self.note_file_vma_mutation();
    }

    pub fn collect_file_vmas(&self) -> Vec<Arc<LockedVMA>> {
        let _guard = self.i_mmap_read();
        self.file_vmas.lock_irqsave().collect_all()
    }

    pub fn collect_file_vmas_in_page_range(
        &self,
        start_page_index: usize,
        end_page_index: usize,
    ) -> Vec<Arc<LockedVMA>> {
        let _guard = self.i_mmap_read();
        self.file_vmas
            .lock_irqsave()
            .collect_all()
            .into_iter()
            .filter(|vma| {
                let guard = vma.lock();
                let Some(vma_pgoff) = guard.backing_page_offset() else {
                    return false;
                };
                let vma_pages = guard.region().size() >> MMArch::PAGE_SHIFT;
                let vma_end = vma_pgoff.saturating_add(vma_pages);
                start_page_index < vma_end && vma_pgoff <= end_page_index
            })
            .collect()
    }

    fn collect_file_vmas_snapshot(
        &self,
        page_range: Option<(usize, Option<usize>)>,
    ) -> (u64, Vec<Arc<LockedVMA>>) {
        let _guard = self.i_mmap_read();
        let seq = self.file_vma_seq();
        let mut vmas = self.file_vmas.lock_irqsave().collect_all();
        if let Some((start_page_index, end_page_index_exclusive)) = page_range {
            vmas.retain(|vma| {
                vma.file_pgoff_intersection(start_page_index, end_page_index_exclusive)
                    .is_some()
            });
        }
        (seq, vmas)
    }

    pub fn collect_mapped_vmas_for_page(&self, page_index: usize) -> Vec<Arc<LockedVMA>> {
        self.collect_file_vmas_in_page_range(page_index, page_index)
    }

    pub fn unmap_mapping_pages(
        &self,
        start_page_index: usize,
        end_page_index_exclusive: Option<usize>,
    ) -> Result<(), SystemError> {
        self.unmap_mapping_pages_with_mode(
            start_page_index,
            end_page_index_exclusive,
            UnmapMappingMode::CacheOnly,
        )
    }

    pub fn unmap_mapping_pages_even_cow(
        &self,
        start_page_index: usize,
        end_page_index_exclusive: Option<usize>,
    ) -> Result<(), SystemError> {
        self.unmap_mapping_pages_with_mode(
            start_page_index,
            end_page_index_exclusive,
            UnmapMappingMode::EvenCow,
        )
    }

    fn unmap_mapping_pages_with_mode(
        &self,
        start_page_index: usize,
        end_page_index_exclusive: Option<usize>,
        mode: UnmapMappingMode,
    ) -> Result<(), SystemError> {
        loop {
            let (seq, snapshot) =
                self.collect_file_vmas_snapshot(Some((start_page_index, end_page_index_exclusive)));
            let mut mm_groups: HashMap<u64, MmFileRangeGroup> = HashMap::new();

            for vma in snapshot {
                let Some(region) =
                    vma.file_pgoff_intersection(start_page_index, end_page_index_exclusive)
                else {
                    continue;
                };
                let Some(mm) = vma.lock().address_space().and_then(|space| space.upgrade()) else {
                    continue;
                };
                mm_groups
                    .entry(mm.id())
                    .or_insert_with(|| MmFileRangeGroup::new(mm.clone()))
                    .ranges
                    .push((vma, region));
            }

            for (_id, group) in mm_groups {
                let mm_guard = group.mm.read();
                let _pt_edit = group.mm.page_table_edit();
                let mut tlb = MmuGather::gather(&group.mm);
                for (vma, region) in group.ranges {
                    vma.unmap_range(region, &mm_guard.user_mapper.utable, &mut tlb, mode);
                }
                tlb.finish();
            }

            if self.file_vma_seq() == seq {
                break;
            }
        }

        Ok(())
    }

    pub fn truncate(&self, new_size: usize) -> Result<(), SystemError> {
        let hole_start_page = page_align_up(new_size) >> MMArch::PAGE_SHIFT;
        loop {
            // Keep the MM lock order out of invalidate_write:
            // first tear down existing PTEs, then block new faults while removing cache pages.
            self.unmap_mapping_pages_even_cow(hole_start_page, None)?;

            let truncate_committed = {
                let _invalidate = self.invalidate_write();
                self.truncate_locked(new_size)?
            };

            if truncate_committed {
                // Match Linux truncate_pagecache(): private COW pages can appear after
                // the first unmap and before cache truncation commits, so unmap again
                // after releasing invalidate_write to preserve the global lock order.
                self.unmap_mapping_pages_even_cow(hole_start_page, None)?;
                return Ok(());
            }
        }
    }

    /// Drop budget tickets whose last frozen page was removed by truncate.
    ///
    /// A budget-saturated WRITE leaves its page Dirty/tagged until a permit is
    /// handed to its retry.  Truncate may remove that page without ever
    /// entering the normal submit/retire path, so cancellation must recheck
    /// both tagged predicates under their shared linearization lock.  A
    /// partially truncated generation that still has a tag or a submission
    /// record remains queued.
    fn cancel_truncated_tagged_writeback_budget_retries(&self) {
        let cancelled = {
            let _tagged_writeback_transition = self.tagged_writeback_lock.lock();
            // This snapshot is deliberately inside the tagged-state
            // transition lock. `arm_tagged_writeback_budget_retry()` holds
            // that same lock across its predicate and insertion, while the
            // final page removal below also holds it. Consequently a drain
            // cannot insert a ticket after truncate removes its last tag but
            // before this sweep observes the cache-side retry map.
            let candidates = {
                let retries = self.tagged_writeback_budget_retries.lock();
                retries
                    .iter()
                    .map(|(epoch, retry)| (*epoch, retry.start_index, retry.frozen_end))
                    .collect::<Vec<_>>()
            };
            candidates
                .into_iter()
                .filter_map(|(epoch, start_index, frozen_end)| {
                    (!PageCacheManager::has_exact_tagged_writeback(
                        self,
                        start_index,
                        frozen_end,
                        epoch,
                    ) && !PageCacheManager::has_exact_tagged_writeback_submission(
                        self,
                        start_index,
                        frozen_end,
                        epoch,
                    ))
                    .then_some(epoch)
                })
                .collect::<Vec<_>>()
        };
        for epoch in cancelled {
            PageCacheManager::cancel_tagged_writeback_budget_retry(self, epoch);
        }
    }

    /// Remove cached pages while the caller holds `invalidate_write()`.
    ///
    /// Filesystems that must serialize their on-disk size update with page
    /// invalidation use this after unmapping PTEs.  Callers must repeat the
    /// unmap-and-lock sequence when this returns `false`.
    pub(crate) fn truncate_locked(&self, new_size: usize) -> Result<bool, SystemError> {
        let first_full_truncate_page = page_align_up(new_size) >> MMArch::PAGE_SHIFT;
        let mut removed_tagged_page = false;
        let truncate_indices: Vec<usize> = {
            let guard = self.inner.lock();
            guard
                .pages
                .keys()
                .copied()
                .filter(|index| *index >= first_full_truncate_page)
                .collect()
        };

        for page_index in truncate_indices {
            loop {
                let entry = {
                    let guard = self.inner.lock();
                    guard.get_entry(page_index)
                };
                let Some(entry) = entry else {
                    break;
                };
                match entry.state() {
                    PageState::Loading => {
                        let _ = entry.wait_ready();
                        continue;
                    }
                    PageState::Writeback => {
                        let _ = entry.wait_queue.wait_until(|| match entry.state() {
                            PageState::Writeback => None,
                            PageState::Error => Some(Err(SystemError::EIO)),
                            _ => Some(Ok(())),
                        });
                        continue;
                    }
                    _ => {}
                }

                if entry.active_users() != 0 {
                    entry.wait_inactive();
                    continue;
                }

                let mut retry_after_unmap = false;
                let removed_page = {
                    let page_guard = entry.page.read();
                    if page_guard.map_count() != 0 {
                        retry_after_unmap = true;
                        None
                    } else {
                        // invalidate_write prevents a new mapping after this
                        // zero-map observation. Drop the page lock before
                        // taking the tagged transition and membership locks,
                        // then revalidate every mutable entry property.
                        // This is the removal side of the retry-ticket
                        // linearization: a budget retry may be inserted only
                        // while it still observes a matching tag or
                        // submission record.
                        drop(page_guard);
                        let _tagged_writeback_transition = self.tagged_writeback_lock.lock();
                        let mut guard = self.inner.lock();
                        let Some(current) = guard.get_entry(page_index) else {
                            break;
                        };
                        if !Arc::ptr_eq(&current, &entry) {
                            continue;
                        }
                        if current.active_users() != 0 {
                            drop(guard);
                            drop(_tagged_writeback_transition);
                            current.wait_inactive();
                            continue;
                        }
                        match current.state() {
                            PageState::Loading => {
                                drop(guard);
                                drop(_tagged_writeback_transition);
                                let _ = current.wait_ready();
                                continue;
                            }
                            PageState::Writeback => {
                                drop(guard);
                                drop(_tagged_writeback_transition);
                                let _ = current.wait_queue.wait_until(|| match current.state() {
                                    PageState::Writeback => None,
                                    PageState::Error => Some(Err(SystemError::EIO)),
                                    _ => Some(Ok(())),
                                });
                                continue;
                            }
                            _ => {
                                let tag = current.writeback_tag();
                                guard.remove_page(page_index).map(|page| (page, tag))
                            }
                        }
                    }
                };

                if retry_after_unmap {
                    return Ok(false);
                }

                if let Some((page, tag)) = removed_page {
                    removed_tagged_page |= tag != 0;
                    self.discard_unlinked_page(&page);
                }
                drop(self.detach_dirty_retention_if_idle());
                break;
            }
        }

        if removed_tagged_page {
            self.cancel_truncated_tagged_writeback_budget_retries();
            // `WAIT_AFTER` may be sleeping on a tag whose page truncate just
            // removed.  The ticket revalidation below and the waiter share
            // the tagged-state lock; publish after both predicates are
            // coherent rather than relying on an unrelated writeback wake.
            PageCacheManager::notify_tagged_writeback_progress(self);
        }

        if new_size > 0 && !new_size.is_multiple_of(MMArch::PAGE_SIZE) {
            let last_page_index = (new_size - 1) >> MMArch::PAGE_SHIFT;
            let last_len = new_size - (last_page_index << MMArch::PAGE_SHIFT);
            loop {
                let entry = {
                    let guard = self.inner.lock();
                    guard.get_entry(last_page_index)
                };
                let Some(entry) = entry else {
                    break;
                };
                match entry.state() {
                    PageState::Loading => {
                        let _ = entry.wait_ready();
                        continue;
                    }
                    PageState::Writeback => {
                        let _ = entry.wait_queue.wait_until(|| match entry.state() {
                            PageState::Writeback => None,
                            PageState::Error => Some(Err(SystemError::EIO)),
                            _ => Some(Ok(())),
                        });
                        continue;
                    }
                    _ => {}
                }

                let mut page_guard = entry.page.write();
                let inner = self.inner.lock();
                let Some(current) = inner.pages.get(&last_page_index) else {
                    continue;
                };
                if !Arc::ptr_eq(current, &entry) {
                    continue;
                }
                match current.state() {
                    PageState::Loading | PageState::Writeback => continue,
                    _ => unsafe {
                        page_guard.truncate(last_len);
                    },
                }
                break;
            }
        }

        Ok(true)
    }

    pub fn mkclean_page(
        &self,
        page_index: usize,
        unmap: bool,
    ) -> Result<Vec<Arc<LockedVMA>>, SystemError> {
        loop {
            let (seq, snapshot) =
                self.collect_file_vmas_snapshot(Some((page_index, Some(page_index + 1))));
            let mut mm_groups: HashMap<u64, MmFilePageGroup> = HashMap::new();

            for vma in snapshot {
                let (Some(mm), Ok(virt)) = ({
                    let guard = vma.lock();
                    (
                        guard.address_space().and_then(|space| space.upgrade()),
                        guard.page_address(page_index),
                    )
                }) else {
                    continue;
                };

                mm_groups
                    .entry(mm.id())
                    .or_insert_with(|| MmFilePageGroup::new(mm.clone()))
                    .items
                    .push((vma, virt));
            }

            let mut unmapped = Vec::new();
            for (_id, group) in mm_groups {
                let mm_guard = group.mm.read();
                let _pt_edit = group.mm.page_table_edit();
                let mut tlb = MmuGather::gather(&group.mm);
                for (vma, virt) in group.items {
                    if unmap {
                        if let Some((_paddr, _flags, flush)) =
                            unsafe { mm_guard.user_mapper.utable.unmap_phys_preserve_tables(virt) }
                        {
                            unsafe { flush.ignore() };
                            tlb.accumulate_range(virt);
                            unmapped.push(vma);
                        }
                        continue;
                    }

                    let Some((_paddr, flags)) = mm_guard.user_mapper.utable.translate(virt) else {
                        continue;
                    };
                    if !flags.has_write() {
                        continue;
                    }
                    if let Some(flush) = unsafe {
                        mm_guard
                            .user_mapper
                            .utable
                            .remap_present(virt, flags.set_write(false).set_dirty(false))
                    } {
                        unsafe { flush.ignore() };
                        tlb.accumulate_range(virt);
                    }
                }
                tlb.finish();
            }

            if self.file_vma_seq() == seq {
                return Ok(unmapped);
            }
        }
    }
}
