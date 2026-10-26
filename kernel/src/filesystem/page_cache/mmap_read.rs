//! Fault-side owner for native reads after the address-space lock is released.

use super::{
    resource_wait, Arc, FaultRetryWait, MMArch, MemoryAccount, MemoryAllocationError,
    MemoryManagementArch, ObjectAccount, PageCache, PageCacheReadBatchCompletion,
    PageCacheReadBatchRequest, PageState, SystemError,
};

struct MmapReadRetryWait {
    cache: Arc<PageCache>,
    page_index: usize,
    window_pages: usize,
    owner: Arc<MemoryAccount>,
    creator: Arc<ObjectAccount>,
}

/// Required preparation for a filesystem fault which cannot perform I/O
/// while the address-space lock is held. Unlike speculative readahead, a
/// demanded page's allocation and I/O errors must reach the fault caller.
#[derive(Debug)]
struct FaultDemandReadWait {
    cache: Arc<PageCache>,
    page_index: usize,
    owner: Arc<MemoryAccount>,
    creator: Arc<ObjectAccount>,
}

impl core::fmt::Debug for MmapReadRetryWait {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("MmapReadRetryWait")
            .field("page_index", &self.page_index)
            .field("window_pages", &self.window_pages)
            .finish()
    }
}

impl PageCache {
    pub(crate) fn fault_demand_retry_wait(
        self: &Arc<Self>,
        page_index: usize,
        owner: &Arc<MemoryAccount>,
        creator: &Arc<ObjectAccount>,
    ) -> Result<Arc<dyn FaultRetryWait>, MemoryAllocationError> {
        page_index.checked_add(1).ok_or(SystemError::EOVERFLOW)?;
        let wait: Arc<dyn FaultRetryWait> = Arc::try_new(FaultDemandReadWait {
            cache: self.clone(),
            page_index,
            owner: owner.clone(),
            creator: creator.clone(),
        })
        .map_err(|_| SystemError::ENOMEM)?;
        Ok(wait)
    }

    /// Prepare an optional cold-fault read outside all address-space and
    /// invalidation guards. `window_pages` includes the demanded page and must
    /// already be clipped to the VMA by the caller. EOF is checked at submit.
    ///
    /// No backend metadata or I/O is consulted here. Failure to allocate this
    /// optimization's owner leaves the ordinary single-page fault available.
    pub(crate) fn mmap_read_retry_wait(
        self: &Arc<Self>,
        page_index: usize,
        window_pages: usize,
        owner: &Arc<MemoryAccount>,
        creator: &Arc<ObjectAccount>,
    ) -> Option<Arc<dyn FaultRetryWait>> {
        let window_pages = window_pages.min(self.backend()?.read_batch_pages());
        if window_pages <= 1 {
            return None;
        }
        page_index.checked_add(window_pages)?;
        let wait: Arc<dyn FaultRetryWait> = Arc::try_new(MmapReadRetryWait {
            cache: self.clone(),
            page_index,
            window_pages,
            owner: owner.clone(),
            creator: creator.clone(),
        })
        .ok()?;
        Some(wait)
    }
}

impl FaultRetryWait for FaultDemandReadWait {
    fn wait(&self) -> Result<(), MemoryAllocationError> {
        MmapReadRetryWait::check_interrupted()?;
        // Submission, backend metadata and the invalidation gate are all
        // reached after the outer fault loop has dropped its MM guard.
        let invalidate = self.cache.invalidate_read();
        let entry = loop {
            if let Some(entry) = self.cache.inner.lock().get_entry(self.page_index) {
                break entry;
            }
            let backend = self.cache.backend().ok_or(SystemError::EIO)?;
            let valid_bytes = backend.read_window_limit(self.page_index, 1)?;
            if valid_bytes == 0 {
                // The outer fault revalidates EOF; no source page exists.
                return MmapReadRetryWait::check_interrupted();
            }
            if valid_bytes > MMArch::PAGE_SIZE {
                return Err(SystemError::EIO.into());
            }
            let domain_io = self.cache.try_acquire_domain_io()?;
            let completion = match PageCacheReadBatchCompletion::try_reserve(
                &self.cache,
                self.page_index,
                1,
                domain_io,
                &self.owner,
                &self.creator,
            ) {
                Ok(completion) => completion,
                Err(MemoryAllocationError::System(SystemError::EEXIST)) => {
                    MmapReadRetryWait::check_interrupted()?;
                    continue;
                }
                Err(error) => return Err(error),
            };
            let entry = completion.slot_entry(0).expect("reserved one demand slot");
            // Capture identity before the backend may synchronously report
            // Error and unlink it. Looking up after submission would confuse
            // that error with a successfully filled, subsequently evicted page.
            backend.submit_read_batch(
                PageCacheReadBatchRequest {
                    start_index: self.page_index,
                    page_count: 1,
                    valid_bytes,
                },
                completion,
            );
            break entry;
        };
        drop(invalidate);
        resource_wait::wait_for_resource_killable(entry.wait_key(), || match entry.state() {
            PageState::Loading => None,
            PageState::Error => Some(Err(SystemError::EIO)),
            _ => Some(Ok(())),
        })??;
        MmapReadRetryWait::check_interrupted()
    }
}

impl MmapReadRetryWait {
    fn check_interrupted() -> Result<(), MemoryAllocationError> {
        let task = crate::process::ProcessManager::current_pcb();
        if resource_wait::WaitMode::Killable.interrupted(&task) {
            return Err(SystemError::ERESTARTSYS.into());
        }
        Ok(())
    }

    /// Preparation failures belong to the ordinary fault retry, including its
    /// existing allocation recovery. Only cancellation escapes this optional
    /// read, so speculative work never starts a second OOM policy.
    fn submit(&self, index: usize, count: usize) -> Result<bool, MemoryAllocationError> {
        Self::check_interrupted()?;
        match self
            .cache
            .try_read_pages(index, count, &self.owner, &self.creator)
        {
            Ok(()) => Ok(true),
            Err(MemoryAllocationError::System(
                error @ (SystemError::EINTR | SystemError::ERESTARTSYS),
            )) => Err(error.into()),
            Err(_) => {
                Self::check_interrupted()?;
                Ok(false)
            }
        }
    }
}

impl FaultRetryWait for MmapReadRetryWait {
    fn wait(&self) -> Result<(), MemoryAllocationError> {
        if !self.submit(self.page_index, 1)? {
            return Ok(());
        }

        // Observe only the entry already submitted (or installed by a racing
        // reader). Do not allocate a replacement if truncate removed it.
        let entry = { self.cache.inner.lock().get_entry(self.page_index) };
        let Some(entry) = entry else {
            return Self::check_interrupted();
        };
        let ready =
            resource_wait::wait_for_resource_killable(entry.wait_key(), || match entry.state() {
                PageState::Loading => None,
                PageState::Error => Some(false),
                _ => Some(true),
            })?;
        // No entry pin or invalidation guard crosses the following admission.
        // The outer fault retry revalidates both cache and VMA before mapping.
        drop(entry);
        if ready {
            let _ = self.submit(self.page_index + 1, self.window_pages - 1)?;
        }
        Self::check_interrupted()
    }
}
