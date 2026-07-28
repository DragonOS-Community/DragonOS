use super::{
    pc_stats, Arc, AtomicU64, AtomicU8, MMArch, ManuallyDrop, MemoryManagementArch, Ordering, Page,
    PageCache, PageCacheManager, PageCacheReadDmaDescriptor, PageCacheReadDmaState, PageEntry,
    PageFlags, PageState, SystemError, Vec,
};

static PAGE_CACHE_DMA_RESERVATION_ID: AtomicU64 = AtomicU64::new(1);

impl PageCacheReadDmaDescriptor {
    pub fn page_index(&self) -> usize {
        self.page_index
    }

    pub fn vaddr(&self) -> crate::mm::VirtAddr {
        self.vaddr
    }

    pub fn paddr(&self) -> crate::mm::PhysAddr {
        self.paddr
    }

    pub fn len(&self) -> usize {
        MMArch::PAGE_SIZE
    }
}

struct PageCacheReadDmaItem {
    descriptor: PageCacheReadDmaDescriptor,
    entry: Arc<PageEntry>,
    page: Arc<Page>,
}

/// Counts live DMA reservations without exposing or changing their state
/// machine. Ownership follows the reservation on every completion/error path.
struct PageCacheReadDmaStatsGuard;

impl PageCacheReadDmaStatsGuard {
    fn acquire() -> Self {
        pc_stats::begin_read_dma_reservation();
        Self
    }
}

impl Drop for PageCacheReadDmaStatsGuard {
    fn drop(&mut self) {
        pc_stats::end_read_dma_reservation();
    }
}

/// Owns candidate pages which are inaccessible to page-cache readers until DMA
/// has retired, the unread tail has been initialized, and each exact marker is
/// published.
pub struct PageCacheReadDmaReservation {
    id: u64,
    cache: Arc<PageCache>,
    state: AtomicU8,
    items: ManuallyDrop<Vec<PageCacheReadDmaItem>>,
    _stats_guard: ManuallyDrop<PageCacheReadDmaStatsGuard>,
    track_direct_read_stats: bool,
}

impl core::fmt::Debug for PageCacheReadDmaReservation {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PageCacheReadDmaReservation")
            .field("id", &self.id)
            .field("state", &self.state())
            .field("pages", &self.items.len())
            .finish()
    }
}

impl Drop for PageCacheReadDmaReservation {
    fn drop(&mut self) {
        if self.state() == PageCacheReadDmaState::Submitted {
            // A submitted owner is required to live in the pending table or reset
            // quarantine. Do not detach its marker here: doing so could disguise
            // a transport lifetime bug and permit a second fill of the same index.
            log::error!(
                "dropping submitted page-cache DMA reservation {} without retirement",
                self.id
            );
            // Intentionally leak the exact page/entry owners. This is a last-line
            // memory-safety guard for a violated transport contract, not a normal
            // timeout path (which must retain the whole reservation in quarantine).
            return;
        }
        self.rollback_markers(SystemError::EIO, true);
        unsafe { ManuallyDrop::drop(&mut self.items) };
        unsafe { ManuallyDrop::drop(&mut self._stats_guard) };
    }
}

impl PageCacheReadDmaReservation {
    pub fn id(&self) -> u64 {
        self.id
    }

    pub fn tracks_direct_read_stats(&self) -> bool {
        self.track_direct_read_stats
    }

    pub fn state(&self) -> PageCacheReadDmaState {
        match self.state.load(Ordering::Acquire) {
            0 => PageCacheReadDmaState::Prepared,
            1 => PageCacheReadDmaState::Submitted,
            2 => PageCacheReadDmaState::Completed,
            3 => PageCacheReadDmaState::ResetRetired,
            _ => unreachable!("invalid page-cache DMA reservation state"),
        }
    }

    pub fn page_count(&self) -> usize {
        self.items.len()
    }

    pub fn payload_capacity(&self) -> usize {
        self.items.len() * MMArch::PAGE_SIZE
    }

    pub fn descriptors(&self) -> impl ExactSizeIterator<Item = PageCacheReadDmaDescriptor> + '_ {
        self.items.iter().map(|item| item.descriptor)
    }

    /// Must be called only after the virtqueue accepted all descriptors.
    pub fn mark_submitted(&self) -> Result<(), SystemError> {
        self.transition(
            PageCacheReadDmaState::Prepared,
            PageCacheReadDmaState::Submitted,
        )
    }

    /// Records that the device can no longer access the pages (matching pop).
    pub fn mark_completed(&self) -> Result<(), SystemError> {
        self.transition(
            PageCacheReadDmaState::Submitted,
            PageCacheReadDmaState::Completed,
        )
    }

    /// Records successful exact-token detach after reset. The owner remains alive
    /// so callers may quarantine it until reset completion is beyond doubt.
    pub fn mark_reset_retired(&self) -> Result<(), SystemError> {
        self.transition(
            PageCacheReadDmaState::Submitted,
            PageCacheReadDmaState::ResetRetired,
        )
    }

    /// Detach cache markers while DMA ownership is still unresolved. The state
    /// deliberately remains Submitted and `self` must be retained in quarantine;
    /// no page content may be accessed on this path.
    pub fn detach_mapping_for_quarantine(&self) -> Result<(), SystemError> {
        if self.state() != PageCacheReadDmaState::Submitted {
            return Err(SystemError::EINVAL);
        }
        self.rollback_markers(SystemError::EIO, false);
        Ok(())
    }

    fn transition(
        &self,
        from: PageCacheReadDmaState,
        to: PageCacheReadDmaState,
    ) -> Result<(), SystemError> {
        if !page_cache_dma_transition_allowed(from, to) {
            return Err(SystemError::EINVAL);
        }
        self.state
            .compare_exchange(from as u8, to as u8, Ordering::AcqRel, Ordering::Acquire)
            .map(|_| ())
            .map_err(|_| SystemError::EINVAL)
    }

    /// Publish a successfully completed payload. Bytes not written by the device
    /// are zeroed through the end of every reserved page before any page is made
    /// visible.
    pub fn publish_completed(&self, payload_len: usize) -> Result<Vec<Arc<Page>>, SystemError> {
        if self.state() != PageCacheReadDmaState::Completed || payload_len > self.payload_capacity()
        {
            return Err(SystemError::EINVAL);
        }

        for (position, item) in self.items.iter().enumerate() {
            let page_payload_start = position * MMArch::PAGE_SIZE;
            let initialized = payload_len
                .saturating_sub(page_payload_start)
                .min(MMArch::PAGE_SIZE);
            if initialized < MMArch::PAGE_SIZE {
                let mut page = item.page.write();
                unsafe { page.as_slice_mut()[initialized..].fill(0) };
            }
            item.page.write().add_flags(PageFlags::PG_UPTODATE);
        }

        let inner = self.cache.inner.lock();
        for item in self.items.iter() {
            let Some(current) = inner.get_entry(item.descriptor.page_index) else {
                drop(inner);
                self.rollback_markers(SystemError::EIO, true);
                return Err(SystemError::EIO);
            };
            if !Arc::ptr_eq(&current, &item.entry)
                || !Arc::ptr_eq(&current.page, &item.page)
                || current.state() != PageState::Loading
            {
                drop(inner);
                self.rollback_markers(SystemError::EIO, true);
                return Err(SystemError::EIO);
            }
        }

        let mut published = Vec::with_capacity(self.items.len());
        for item in self.items.iter() {
            let current = inner
                .get_entry(item.descriptor.page_index)
                .expect("DMA reservation identity was validated under the same lock");
            current.account_state_transition(PageState::Loading, PageState::UpToDate);
            current.set_state(PageState::UpToDate);
            published.push(item.page.clone());
            current.wait_queue.wake_all();
        }
        drop(inner);
        Ok(published)
    }

    /// Complete the same reserved read through the bounded contiguous fallback.
    ///
    /// No device has observed these pages while they are Prepared. Copy the one reply into the
    /// final candidate pages, then reuse the same tail-zeroing and identity-checked publication
    /// path as a direct DMA completion.
    pub fn publish_contiguous(&self, payload: &[u8]) -> Result<Vec<Arc<Page>>, SystemError> {
        if payload.len() > self.payload_capacity() {
            return Err(SystemError::EINVAL);
        }
        self.state
            .compare_exchange(
                PageCacheReadDmaState::Prepared as u8,
                PageCacheReadDmaState::Completed as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .map_err(|_| SystemError::EINVAL)?;

        for (position, item) in self.items.iter().enumerate() {
            let start = position * MMArch::PAGE_SIZE;
            let len = payload.len().saturating_sub(start).min(MMArch::PAGE_SIZE);
            if len == 0 {
                break;
            }
            let mut page = item.page.write();
            unsafe { page.as_slice_mut()[..len].copy_from_slice(&payload[start..start + len]) };
        }
        self.publish_completed(payload.len())
    }

    /// Remove all still-matching markers. Candidate pages stay owned by `self`;
    /// this is important for reset-time quarantine.
    pub fn rollback(&self, error: SystemError) -> Result<(), SystemError> {
        match self.state() {
            PageCacheReadDmaState::Prepared
            | PageCacheReadDmaState::Completed
            | PageCacheReadDmaState::ResetRetired => {
                self.rollback_markers(error, true);
                Ok(())
            }
            PageCacheReadDmaState::Submitted => Err(SystemError::EBUSY),
        }
    }

    fn rollback_markers(&self, _error: SystemError, discard_pages: bool) {
        for item in self.items.iter() {
            let removed = {
                let mut inner = self.cache.inner.lock();
                let matches = inner
                    .get_entry(item.descriptor.page_index)
                    .map(|current| {
                        Arc::ptr_eq(&current, &item.entry)
                            && Arc::ptr_eq(&current.page, &item.page)
                            && current.state() == PageState::Loading
                    })
                    .unwrap_or(false);
                if matches {
                    inner.remove_page(item.descriptor.page_index);
                    true
                } else {
                    false
                }
            };
            if removed {
                item.entry.set_state(PageState::Error);
                item.entry.wait_queue.wake_all();
                if discard_pages {
                    self.cache.discard_unlinked_page(&item.page);
                }
            }
        }
    }

    fn cleanup_unsubmitted_items(cache: &Arc<PageCache>, items: &[PageCacheReadDmaItem]) {
        for item in items {
            let mut inner = cache.inner.lock();
            if matches!(inner.get_entry(item.descriptor.page_index), Some(current) if Arc::ptr_eq(&current, &item.entry))
            {
                inner.remove_page(item.descriptor.page_index);
                item.entry.set_state(PageState::Error);
                item.entry.wait_queue.wake_all();
            }
            drop(inner);
            cache.discard_unlinked_page(&item.page);
        }
    }
}

fn page_cache_dma_transition_allowed(
    from: PageCacheReadDmaState,
    to: PageCacheReadDmaState,
) -> bool {
    matches!(
        (from, to),
        (
            PageCacheReadDmaState::Prepared,
            PageCacheReadDmaState::Submitted
        ) | (
            PageCacheReadDmaState::Submitted,
            PageCacheReadDmaState::Completed
        ) | (
            PageCacheReadDmaState::Submitted,
            PageCacheReadDmaState::ResetRetired
        )
    )
}

#[cfg(test)]
mod page_cache_dma_state_tests {
    use super::{page_cache_dma_transition_allowed, PageCacheReadDmaState::*};

    #[test]
    fn accepts_only_submission_and_dma_retirement_edges() {
        assert!(page_cache_dma_transition_allowed(Prepared, Submitted));
        assert!(page_cache_dma_transition_allowed(Submitted, Completed));
        assert!(page_cache_dma_transition_allowed(Submitted, ResetRetired));

        for state in [Prepared, Submitted, Completed, ResetRetired] {
            assert!(!page_cache_dma_transition_allowed(state, state));
        }
        assert!(!page_cache_dma_transition_allowed(Prepared, Completed));
        assert!(!page_cache_dma_transition_allowed(Prepared, ResetRetired));
        assert!(!page_cache_dma_transition_allowed(Completed, Submitted));
        assert!(!page_cache_dma_transition_allowed(ResetRetired, Submitted));
    }
}

impl PageCacheManager {
    pub fn reserve_read_dma(
        &self,
        start_page_index: usize,
        page_count: usize,
        track_direct_read_stats: bool,
    ) -> Result<PageCacheReadDmaReservation, SystemError> {
        if page_count == 0 || start_page_index.checked_add(page_count - 1).is_none() {
            return Err(SystemError::EINVAL);
        }

        let cache = self.upgrade()?;
        let stats_guard = PageCacheReadDmaStatsGuard::acquire();
        let page_cache_ref = {
            let inner = cache.inner.lock();
            if (0..page_count).any(|offset| inner.get_entry(start_page_index + offset).is_some()) {
                return Err(SystemError::EEXIST);
            }
            inner.page_cache_ref.clone()
        };
        let mut items: Vec<PageCacheReadDmaItem> = Vec::with_capacity(page_count);

        for offset in 0..page_count {
            let page_index = start_page_index + offset;
            let page = match cache.allocate_page(page_cache_ref.clone(), page_index) {
                Ok(page) => page,
                Err(error) => {
                    PageCacheReadDmaReservation::cleanup_unsubmitted_items(&cache, &items);
                    return Err(error);
                }
            };
            let paddr = page.phys_address();
            let Some(vaddr) = (unsafe { MMArch::phys_2_virt(paddr) }) else {
                cache.discard_unlinked_page(&page);
                PageCacheReadDmaReservation::cleanup_unsubmitted_items(&cache, &items);
                return Err(SystemError::EFAULT);
            };
            let entry = Arc::new(PageEntry::new(page.clone(), PageState::Loading));
            items.push(PageCacheReadDmaItem {
                descriptor: PageCacheReadDmaDescriptor {
                    page_index,
                    vaddr,
                    paddr,
                },
                entry,
                page,
            });
        }

        // Publish the complete Loading range atomically.  Exposing a prefix
        // while allocating later pages lets a waiter attach to that prefix and
        // then observe a synthetic EIO when a later-page conflict rolls it back.
        let mut inner = cache.inner.lock();
        if items
            .iter()
            .any(|item| inner.get_entry(item.descriptor.page_index).is_some())
        {
            drop(inner);
            PageCacheReadDmaReservation::cleanup_unsubmitted_items(&cache, &items);
            return Err(SystemError::EEXIST);
        }
        for item in &items {
            if let Err(error) = inner.insert_entry(item.descriptor.page_index, item.entry.clone()) {
                drop(inner);
                PageCacheReadDmaReservation::cleanup_unsubmitted_items(&cache, &items);
                return Err(error);
            }
        }
        drop(inner);
        for item in &items {
            cache.reconcile_entry_unevictable_for_insert(&item.entry);
        }

        Ok(PageCacheReadDmaReservation {
            id: PAGE_CACHE_DMA_RESERVATION_ID.fetch_add(1, Ordering::Relaxed),
            cache,
            state: AtomicU8::new(PageCacheReadDmaState::Prepared as u8),
            items: ManuallyDrop::new(items),
            _stats_guard: ManuallyDrop::new(stats_guard),
            track_direct_read_stats,
        })
    }
}

impl PageCache {
    pub fn wait_read_dma_conflict(
        &self,
        start_page_index: usize,
        page_count: usize,
    ) -> Result<bool, SystemError> {
        let entry = {
            let inner = self.inner.lock();
            (0..page_count)
                .find_map(|offset| inner.get_entry(start_page_index.saturating_add(offset)))
        };
        let Some(entry) = entry else {
            return Ok(false);
        };
        let _ = entry.wait_ready()?;
        Ok(true)
    }
}
