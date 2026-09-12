use super::{
    schedule_work, Arc, AtomicUsize, MMArch, MemoryManagementArch, Ordering, Page, PageCache,
    PageCacheBackend, PageCacheDomainIoPermit, PageEntry, PageFlags, PageState, SystemError, Vec,
    Work,
};

/// Stable bounds passed to one backend submission. `valid_bytes` is relative
/// to `start_index` and may end in the middle of the final page.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PageCacheReadBatchRequest {
    pub start_index: usize,
    pub page_count: usize,
    pub valid_bytes: usize,
}

struct LoadingSlot {
    page_index: usize,
    entry: Arc<PageEntry>,
    page: Arc<Page>,
    terminal: core::sync::atomic::AtomicBool,
}

struct LoadingBatch {
    cache: Arc<PageCache>,
    slots: Vec<LoadingSlot>,
    unfinished: AtomicUsize,
    _domain_io: Option<PageCacheDomainIoPermit>,
}

/// The sole page-state publication authority for one backend read batch.
/// Device owners may only report results through this object; they never see
/// PageCache's map or mutate PageState directly.
pub struct PageCacheReadBatchCompletion {
    inner: Arc<LoadingBatch>,
}

impl PageCacheReadBatchCompletion {
    pub(super) fn reserve(
        cache: &Arc<PageCache>,
        start_index: usize,
        page_count: usize,
        domain_io: Option<PageCacheDomainIoPermit>,
    ) -> Result<Self, SystemError> {
        if page_count == 0 || start_index.checked_add(page_count - 1).is_none() {
            return Err(SystemError::EINVAL);
        }
        let page_cache_ref = {
            let inner = cache.inner.lock();
            if (0..page_count).any(|offset| inner.get_entry(start_index + offset).is_some()) {
                return Err(SystemError::EEXIST);
            }
            inner.page_cache_ref.clone()
        };

        let mut slots = Vec::new();
        slots
            .try_reserve_exact(page_count)
            .map_err(|_| SystemError::ENOMEM)?;
        for offset in 0..page_count {
            let page_index = start_index + offset;
            let page = match cache.allocate_page(page_cache_ref.clone(), page_index) {
                Ok(page) => page,
                Err(error) => {
                    Self::discard_unsubmitted(cache, &slots);
                    return Err(error);
                }
            };
            let entry = Arc::new(PageEntry::new(page.clone(), PageState::Loading));
            slots.push(LoadingSlot {
                page_index,
                entry,
                page,
                terminal: core::sync::atomic::AtomicBool::new(false),
            });
        }

        // Publish the whole empty subrange atomically. A concurrent reader
        // must never attach to a prefix which is later rolled back because a
        // later page conflicted.
        let mut inner = cache.inner.lock();
        if slots
            .iter()
            .any(|slot| inner.get_entry(slot.page_index).is_some())
        {
            drop(inner);
            Self::discard_unsubmitted(cache, &slots);
            return Err(SystemError::EEXIST);
        }
        for slot in &slots {
            if let Err(error) = inner.insert_entry(slot.page_index, slot.entry.clone()) {
                drop(inner);
                Self::discard_unsubmitted(cache, &slots);
                return Err(error);
            }
        }
        drop(inner);
        for slot in &slots {
            cache.reconcile_entry_unevictable_for_insert(&slot.entry);
        }

        Ok(Self {
            inner: Arc::new(LoadingBatch {
                cache: cache.clone(),
                unfinished: AtomicUsize::new(page_count),
                slots,
                _domain_io: domain_io,
            }),
        })
    }

    pub fn page_count(&self) -> usize {
        self.inner.slots.len()
    }

    /// Publish one full-block payload range. `last_page_valid_bytes` is only
    /// used for the final EOF page; all bytes after it are zeroed before the
    /// page becomes visible.
    pub fn complete_data(
        &self,
        first_slot: usize,
        page_count: usize,
        payload: &[u8],
        last_page_valid_bytes: usize,
    ) -> Result<(), SystemError> {
        self.validate_range(first_slot, page_count)?;
        let expected = page_count
            .checked_mul(MMArch::PAGE_SIZE)
            .ok_or(SystemError::EOVERFLOW)?;
        if payload.len() != expected
            || last_page_valid_bytes == 0
            || last_page_valid_bytes > MMArch::PAGE_SIZE
        {
            return Err(SystemError::EINVAL);
        }
        for offset in 0..page_count {
            let slot_index = first_slot + offset;
            let valid = if offset + 1 == page_count {
                last_page_valid_bytes
            } else {
                MMArch::PAGE_SIZE
            };
            let start = offset * MMArch::PAGE_SIZE;
            self.complete_one_data(
                slot_index,
                &payload[start..start + MMArch::PAGE_SIZE],
                valid,
            )?;
        }
        Ok(())
    }

    pub fn complete_zero(
        &self,
        first_slot: usize,
        page_count: usize,
        last_page_valid_bytes: usize,
    ) -> Result<(), SystemError> {
        self.validate_range(first_slot, page_count)?;
        if last_page_valid_bytes == 0 || last_page_valid_bytes > MMArch::PAGE_SIZE {
            return Err(SystemError::EINVAL);
        }
        for offset in 0..page_count {
            let slot = &self.inner.slots[first_slot + offset];
            if !Self::claim(slot)? {
                continue;
            }
            {
                let mut page = slot.page.write();
                unsafe { page.as_slice_mut().fill(0) };
                page.add_flags(PageFlags::PG_UPTODATE);
            }
            self.publish_ready(slot);
        }
        Ok(())
    }

    pub fn complete_error(
        &self,
        first_slot: usize,
        page_count: usize,
        _error: SystemError,
    ) -> Result<(), SystemError> {
        self.validate_range(first_slot, page_count)?;
        for offset in 0..page_count {
            let slot = &self.inner.slots[first_slot + offset];
            if !Self::claim(slot)? {
                continue;
            }
            slot.page.write().add_flags(PageFlags::PG_ERROR);
            slot.entry.set_state(PageState::Error);
            slot.entry.wait_queue.wake_all();
            self.remove_failed(slot);
        }
        Ok(())
    }

    pub fn unfinished(&self) -> usize {
        self.inner.unfinished.load(Ordering::Acquire)
    }

    fn complete_one_data(
        &self,
        slot_index: usize,
        payload: &[u8],
        valid: usize,
    ) -> Result<(), SystemError> {
        let slot = &self.inner.slots[slot_index];
        if !Self::claim(slot)? {
            return Ok(());
        }
        {
            let mut page = slot.page.write();
            let dst = unsafe { page.as_slice_mut() };
            dst[..valid].copy_from_slice(&payload[..valid]);
            dst[valid..].fill(0);
            page.add_flags(PageFlags::PG_UPTODATE);
        }
        self.publish_ready(slot);
        Ok(())
    }

    fn claim(slot: &LoadingSlot) -> Result<bool, SystemError> {
        slot.terminal
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map(|_| true)
            .map_err(|_| SystemError::EIO)
    }

    fn publish_ready(&self, slot: &LoadingSlot) {
        let inner = self.inner.cache.inner.lock();
        let matches = inner.get_entry(slot.page_index).is_some_and(|current| {
            Arc::ptr_eq(&current, &slot.entry)
                && Arc::ptr_eq(&current.page, &slot.page)
                && current.state() == PageState::Loading
        });
        if matches {
            slot.entry
                .account_state_transition(PageState::Loading, PageState::UpToDate);
            slot.entry.set_state(PageState::UpToDate);
            slot.entry.wait_queue.wake_all();
        }
        drop(inner);
        self.finish_one();
    }

    fn remove_failed(&self, slot: &LoadingSlot) {
        let removed = {
            let mut inner = self.inner.cache.inner.lock();
            if inner
                .get_entry(slot.page_index)
                .is_some_and(|current| Arc::ptr_eq(&current, &slot.entry))
            {
                inner.remove_page(slot.page_index);
                true
            } else {
                false
            }
        };
        if removed {
            self.inner.cache.discard_unlinked_page(&slot.page);
        }
        self.finish_one();
    }

    fn finish_one(&self) {
        let previous = self.inner.unfinished.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(previous > 0);
    }

    fn validate_range(&self, first_slot: usize, page_count: usize) -> Result<(), SystemError> {
        if page_count == 0
            || first_slot
                .checked_add(page_count)
                .is_none_or(|end| end > self.inner.slots.len())
        {
            return Err(SystemError::EINVAL);
        }
        Ok(())
    }

    fn discard_unsubmitted(cache: &Arc<PageCache>, slots: &[LoadingSlot]) {
        for slot in slots {
            let removed = {
                let mut inner = cache.inner.lock();
                if inner
                    .get_entry(slot.page_index)
                    .is_some_and(|current| Arc::ptr_eq(&current, &slot.entry))
                {
                    inner.remove_page(slot.page_index);
                    true
                } else {
                    false
                }
            };
            if removed {
                slot.entry.set_state(PageState::Error);
                slot.entry.wait_queue.wake_all();
            }
            cache.discard_unlinked_page(&slot.page);
        }
    }
}

impl Drop for LoadingBatch {
    fn drop(&mut self) {
        for slot in &self.slots {
            if slot.terminal.load(Ordering::Acquire) {
                continue;
            }
            slot.page.write().add_flags(PageFlags::PG_ERROR);
            slot.entry.set_state(PageState::Error);
            slot.entry.wait_queue.wake_all();
            let removed = {
                let mut inner = self.cache.inner.lock();
                if inner
                    .get_entry(slot.page_index)
                    .is_some_and(|current| Arc::ptr_eq(&current, &slot.entry))
                {
                    inner.remove_page(slot.page_index);
                    true
                } else {
                    false
                }
            };
            if removed {
                self.cache.discard_unlinked_page(&slot.page);
            }
        }
    }
}

/// Default compatibility path used by filesystems without a native batch
/// implementation. The Arc receiver makes the queued owner explicit.
pub(super) fn submit_default_read_batch<B: PageCacheBackend + ?Sized + 'static>(
    backend: Arc<B>,
    request: PageCacheReadBatchRequest,
    completion: PageCacheReadBatchCompletion,
) {
    let work = Work::new(move || {
        for offset in 0..request.page_count {
            let mut payload = [0u8; MMArch::PAGE_SIZE];
            let page_index = request.start_index + offset;
            match backend.read_page(page_index, &mut payload) {
                Ok(len) if len <= MMArch::PAGE_SIZE => {
                    let valid = if len == 0 { MMArch::PAGE_SIZE } else { len };
                    let _ = completion.complete_data(offset, 1, &payload, valid);
                }
                Ok(_) => {
                    let _ = completion.complete_error(offset, 1, SystemError::EIO);
                }
                Err(error) => {
                    let _ = completion.complete_error(offset, 1, error);
                }
            }
        }
    });
    schedule_work(work);
}
