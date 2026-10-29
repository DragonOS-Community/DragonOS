//! Exercise production read publication while the global cleanup worker is busy.
use super::*;

const TIMEOUT: Duration = Duration::from_secs(2);

struct GlobalWorkerGate {
    started: AtomicBool,
    release: AtomicBool,
    finished: AtomicBool,
    wait: WaitQueue,
}

struct ReleaseGlobalWorker(Arc<GlobalWorkerGate>);

impl Drop for ReleaseGlobalWorker {
    fn drop(&mut self) {
        self.0.release.store(true, Ordering::Release);
        self.0.wait.wake_all();
    }
}

#[derive(Debug)]
struct ShortReadBackend {
    calls: [AtomicUsize; 2],
}

impl ShortReadBackend {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            calls: [AtomicUsize::new(0), AtomicUsize::new(0)],
        })
    }
}

impl PageCacheBackend for ShortReadBackend {
    fn read_page(&self, index: usize, buffer: &mut [u8]) -> Result<usize, SystemError> {
        self.calls[index].fetch_add(1, Ordering::AcqRel);
        // Poison the tail too: publication, not zero-initialized allocation,
        // must sanitize bytes beyond the reported short read.
        buffer.fill(0xa5);
        if index == 0 {
            buffer[..17].fill(0x5a);
            Ok(17)
        } else {
            Err(SystemError::EIO)
        }
    }

    fn write_page(&self, _: usize, buffer: &[u8]) -> Result<usize, SystemError> {
        Ok(buffer.len())
    }

    fn npages(&self) -> usize {
        2
    }
}

fn ready_before_release(entry: &Arc<PageEntry>) -> bool {
    let deadline = crate::time::Instant::now() + TIMEOUT;
    while entry.state() == PageState::Loading {
        if crate::time::Instant::now() >= deadline {
            return false;
        }
        crate::sched::sched_yield();
    }
    true
}

fn short_read_valid(entry: &Arc<PageEntry>) -> bool {
    if entry.state() != PageState::UpToDate {
        return false;
    }
    let page = entry.page.read();
    let bytes = unsafe { page.as_slice() };
    page.flags().contains(PageFlags::PG_UPTODATE)
        && bytes[..17].iter().all(|byte| *byte == 0x5a)
        && bytes[17..].iter().all(|byte| *byte == 0)
}

fn one_shot_terminal(complete_first: bool) -> Result<bool, SystemError> {
    let waiter = PageIoWaiter::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let valid_result = Arc::new(AtomicBool::new(false));
    if complete_first {
        waiter.complete(Ok(17));
        waiter.complete(Err(SystemError::EIO));
    }
    let callback_calls = calls.clone();
    let callback_valid = valid_result.clone();
    waiter.register_terminal(Box::new(move |result| {
        callback_calls.fetch_add(1, Ordering::AcqRel);
        callback_valid.store(matches!(result, Ok(17)), Ordering::Release);
    }))?;
    if !complete_first {
        waiter.complete(Ok(17));
        waiter.complete(Err(SystemError::EIO));
    }
    let duplicate_rejected = matches!(
        waiter.register_terminal(Box::new(|_| {})),
        Err(SystemError::EEXIST)
    );
    Ok(duplicate_rejected
        && calls.load(Ordering::Acquire) == 1
        && valid_result.load(Ordering::Acquire))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MmapReadOutcome {
    Ready,
    DemandError,
    NeighborError,
    PendingDemand,
}

/// Exercise the native submission contract, rather than the default per-page
/// worker. Pending ownership is released explicitly, including timeout paths.
struct MmapReadBackend {
    outcome: MmapReadOutcome,
    bytes: usize,
    demand_calls: AtomicUsize,
    neighbor_pages: AtomicUsize,
    fallback_calls: AtomicUsize,
    pending: SpinLock<(
        bool,
        Option<(PageCacheReadBatchRequest, PageCacheReadBatchCompletion)>,
    )>,
}

impl core::fmt::Debug for MmapReadBackend {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("MmapReadBackend").finish()
    }
}

impl MmapReadBackend {
    fn new(outcome: MmapReadOutcome, bytes: usize) -> Arc<Self> {
        Arc::new(Self {
            outcome,
            bytes,
            demand_calls: AtomicUsize::new(0),
            neighbor_pages: AtomicUsize::new(0),
            fallback_calls: AtomicUsize::new(0),
            pending: SpinLock::new((outcome == MmapReadOutcome::PendingDemand, None)),
        })
    }

    fn publish(request: PageCacheReadBatchRequest, completion: PageCacheReadBatchCompletion) {
        let payload = vec![0x5a; request.page_count * MMArch::PAGE_SIZE];
        let tail = request.valid_bytes - (request.page_count - 1) * MMArch::PAGE_SIZE;
        let _ = completion.complete_data(0, request.page_count, &payload, tail);
    }

    fn release_pending(&self) {
        let pending = {
            let mut state = self.pending.lock_irqsave();
            state.0 = false;
            state.1.take()
        };
        if let Some((request, completion)) = pending {
            Self::publish(request, completion);
        }
    }
}

impl PageCacheBackend for MmapReadBackend {
    fn read_page(&self, _: usize, buffer: &mut [u8]) -> Result<usize, SystemError> {
        self.fallback_calls.fetch_add(1, Ordering::AcqRel);
        buffer.fill(0x5a);
        Ok(buffer.len())
    }

    fn write_page(&self, _: usize, _: &[u8]) -> Result<usize, SystemError> {
        Err(SystemError::EROFS)
    }

    fn npages(&self) -> usize {
        self.bytes.div_ceil(MMArch::PAGE_SIZE)
    }

    fn read_batch_pages(&self) -> usize {
        4
    }

    fn read_window_limit(&self, start: usize, count: usize) -> Result<usize, SystemError> {
        Ok(self
            .bytes
            .saturating_sub(start * MMArch::PAGE_SIZE)
            .min(count * MMArch::PAGE_SIZE))
    }

    fn submit_read_batch(
        self: Arc<Self>,
        request: PageCacheReadBatchRequest,
        completion: PageCacheReadBatchCompletion,
    ) {
        let demand = request.start_index == 0;
        if demand {
            self.demand_calls.fetch_add(1, Ordering::AcqRel);
        } else {
            self.neighbor_pages
                .fetch_add(request.page_count, Ordering::AcqRel);
        }
        if (demand && self.outcome == MmapReadOutcome::DemandError)
            || (!demand && self.outcome == MmapReadOutcome::NeighborError)
        {
            let _ = completion.complete_error(0, request.page_count, SystemError::EIO);
            return;
        }
        if demand {
            let mut state = self.pending.lock_irqsave();
            if state.0 {
                state.1 = Some((request, completion));
                return;
            }
        }
        Self::publish(request, completion);
    }
}

fn mmap_read_retry() -> Result<bool, SystemError> {
    use crate::cgroup::memory::MemoryPolicy;
    let owner = MemoryAccount::new(0, None)?;
    let creator = ObjectAccount::new(&owner)?;
    let empty_backend = MmapReadBackend::new(MmapReadOutcome::Ready, 0);
    let empty = PageCache::new_unowned(None, Some(empty_backend.clone()));
    let Some(empty_wait) = empty.mmap_read_retry_wait(0, 4, &owner, &creator) else {
        return Ok(false);
    };
    if empty_wait.wait().is_err()
        || empty.manager().pages_count()? != 0
        || empty_backend.demand_calls.load(Ordering::Acquire) != 0
        || empty_backend.neighbor_pages.load(Ordering::Acquire) != 0
        || empty
            .mmap_read_retry_wait(usize::MAX, 4, &owner, &creator)
            .is_some()
    {
        return Ok(false);
    }
    for outcome in [MmapReadOutcome::DemandError, MmapReadOutcome::NeighborError] {
        let backend = MmapReadBackend::new(outcome, 4 * MMArch::PAGE_SIZE);
        let cache = PageCache::new_unowned(None, Some(backend.clone()));
        let Some(wait) = cache.mmap_read_retry_wait(0, 4, &owner, &creator) else {
            return Ok(false);
        };
        if wait.wait().is_err() || backend.demand_calls.load(Ordering::Acquire) != 1 {
            return Ok(false);
        }
        if outcome == MmapReadOutcome::DemandError {
            if backend.neighbor_pages.load(Ordering::Acquire) != 0
                || cache.manager().pages_count()? != 0
            {
                return Ok(false);
            }
        } else if !cache.fault_page_state(0).ready
            || cache.manager().pages_count()? != 1
            || backend.neighbor_pages.load(Ordering::Acquire) != 3
        {
            return Ok(false);
        }
    }

    // Native EOF clipping must zero the partial tail and never reserve page 3.
    let backend = MmapReadBackend::new(MmapReadOutcome::Ready, 2 * MMArch::PAGE_SIZE + 17);
    let cache = PageCache::new_unowned(None, Some(backend.clone()));
    let Some(wait) = cache.mmap_read_retry_wait(0, 4, &owner, &creator) else {
        return Ok(false);
    };
    wait.wait().map_err(PageCache::legacy_allocation_error)?;
    let tail = cache.inner.lock().get_entry(2).ok_or(SystemError::EIO)?;
    if !short_read_valid(&tail)
        || cache.fault_page_state(3).cached
        || backend.neighbor_pages.load(Ordering::Acquire) != 2
        || cache.mmap_read_retry_wait(0, 1, &owner, &creator).is_some()
    {
        return Ok(false);
    }

    // An advisory charge refusal must leave no Loading page and no data charge;
    // relaxing that limit must still permit the ordinary demand fill.
    let limited = MemoryAccount::new(0, Some(owner.clone()))?;
    limited.set_policy(MemoryPolicy {
        max: Some(0),
        ..Default::default()
    });
    let backend = MmapReadBackend::new(MmapReadOutcome::Ready, 4 * MMArch::PAGE_SIZE);
    let cache = PageCache::new_unowned(None, Some(backend.clone()));
    let Some(wait) = cache.mmap_read_retry_wait(0, 4, &limited, &creator) else {
        return Ok(false);
    };
    if wait.wait().is_err()
        || limited.current_pages() != 0
        || cache.manager().pages_count()? != 0
        || backend.demand_calls.load(Ordering::Acquire) != 0
    {
        return Ok(false);
    }
    limited.set_policy(MemoryPolicy::default());
    let page = cache
        .manager()
        .try_commit_page_pinned(0, &limited, &creator)
        .map_err(PageCache::legacy_allocation_error)?;
    drop(page);
    if backend.fallback_calls.load(Ordering::Acquire) != 1 {
        return Ok(false);
    }

    let backend = MmapReadBackend::new(MmapReadOutcome::PendingDemand, 4 * MMArch::PAGE_SIZE);
    let cache = PageCache::new_unowned(None, Some(backend.clone()));
    let Some(wait) = cache.mmap_read_retry_wait(0, 4, &owner, &creator) else {
        return Ok(false);
    };
    pending_read_wait(cache, backend, wait, 3)
}

fn pending_read_wait(
    cache: Arc<PageCache>,
    backend: Arc<MmapReadBackend>,
    wait: Arc<dyn FaultRetryWait>,
    neighbor_pages: usize,
) -> Result<bool, SystemError> {
    let finished = Arc::new(AtomicBool::new(false));
    let succeeded = Arc::new(AtomicBool::new(false));
    let worker_finished = finished.clone();
    let worker_succeeded = succeeded.clone();
    let worker_task = Arc::new(SpinLock::new(
        None::<Arc<crate::process::ProcessControlBlock>>,
    ));
    let task_slot = worker_task.clone();
    schedule_pagecache_io(Work::new(move || {
        *task_slot.lock_irqsave() = Some(crate::process::ProcessManager::current_pcb());
        worker_succeeded.store(wait.wait().is_ok(), Ordering::Release);
        worker_finished.store(true, Ordering::Release);
    }));
    let deadline = crate::time::Instant::now() + TIMEOUT;
    let held = loop {
        // Submission alone does not prove that the token waits: observe its
        // actual blocked task before allowing the native completion to run.
        let blocked = worker_task
            .lock_irqsave()
            .as_ref()
            .is_some_and(|task| task.sched_info().state().is_blocked());
        if blocked && backend.pending.lock_irqsave().1.is_some() {
            break !finished.load(Ordering::Acquire)
                && !cache.fault_page_state(0).ready
                && backend.neighbor_pages.load(Ordering::Acquire) == 0;
        }
        if finished.load(Ordering::Acquire) || crate::time::Instant::now() >= deadline {
            break false;
        }
        crate::sched::sched_yield();
    };
    // Close admission before releasing the captured completion, so a delayed
    // worker cannot publish an unowned pending request after a test timeout.
    backend.release_pending();
    let deadline = crate::time::Instant::now() + TIMEOUT;
    while !finished.load(Ordering::Acquire) && crate::time::Instant::now() < deadline {
        crate::sched::sched_yield();
    }
    Ok(held
        && finished.load(Ordering::Acquire)
        && succeeded.load(Ordering::Acquire)
        && cache.fault_page_state(0).ready
        && backend.neighbor_pages.load(Ordering::Acquire) == neighbor_pages)
}

fn fault_demand_read() -> Result<bool, SystemError> {
    use crate::cgroup::memory::MemoryPolicy;
    let owner = MemoryAccount::new(0, None)?;
    let creator = ObjectAccount::new(&owner)?;
    for outcome in [MmapReadOutcome::Ready, MmapReadOutcome::DemandError] {
        let backend = MmapReadBackend::new(outcome, 17);
        let cache = PageCache::new_unowned(None, Some(backend.clone()));
        let wait = cache
            .fault_demand_retry_wait(0, &owner, &creator)
            .map_err(PageCache::legacy_allocation_error)?;
        let result = wait.wait();
        if outcome == MmapReadOutcome::DemandError {
            // Synchronous completion already removed its failed cache entry.
            // The token must still observe that exact incarnation's error.
            if !matches!(result, Err(MemoryAllocationError::System(SystemError::EIO)))
                || cache.fault_page_state(0).cached
            {
                return Ok(false);
            }
        } else {
            result.map_err(PageCache::legacy_allocation_error)?;
            let entry = cache.inner.lock().get_entry(0).ok_or(SystemError::EIO)?;
            if !short_read_valid(&entry) {
                return Ok(false);
            }
            wait.wait().map_err(PageCache::legacy_allocation_error)?;
        }
        if backend.demand_calls.load(Ordering::Acquire) != 1
            || backend.neighbor_pages.load(Ordering::Acquire) != 0
            || backend.fallback_calls.load(Ordering::Acquire) != 0
        {
            return Ok(false);
        }
    }
    let limited = MemoryAccount::new(0, Some(owner.clone()))?;
    limited.set_policy(MemoryPolicy {
        max: Some(0),
        ..Default::default()
    });
    let backend = MmapReadBackend::new(MmapReadOutcome::Ready, MMArch::PAGE_SIZE);
    let cache = PageCache::new_unowned(None, Some(backend.clone()));
    let wait = cache
        .fault_demand_retry_wait(0, &limited, &creator)
        .map_err(PageCache::legacy_allocation_error)?;
    if !matches!(wait.wait(), Err(MemoryAllocationError::Charge(_)))
        || limited.current_pages() != 0
        || cache.fault_page_state(0).cached
        || backend.demand_calls.load(Ordering::Acquire) != 0
    {
        return Ok(false);
    }
    let backend = MmapReadBackend::new(MmapReadOutcome::PendingDemand, 17);
    let cache = PageCache::new_unowned(None, Some(backend.clone()));
    let wait = cache
        .fault_demand_retry_wait(0, &owner, &creator)
        .map_err(PageCache::legacy_allocation_error)?;
    pending_read_wait(cache, backend, wait, 0)
}

pub(super) fn run() -> Result<bool, SystemError> {
    if !fault_demand_read()? {
        return Ok(false);
    }
    if !mmap_read_retry()? {
        return Ok(false);
    }
    if !one_shot_terminal(false)? || !one_shot_terminal(true)? {
        return Ok(false);
    }
    let gate = Arc::new(GlobalWorkerGate {
        started: AtomicBool::new(false),
        release: AtomicBool::new(false),
        finished: AtomicBool::new(false),
        wait: WaitQueue::default(),
    });
    let release = ReleaseGlobalWorker(gate.clone());
    let worker_gate = gate.clone();
    schedule_work(Work::new(move || {
        worker_gate.started.store(true, Ordering::Release);
        worker_gate.wait.wake_all();
        worker_gate
            .wait
            .wait_until(|| worker_gate.release.load(Ordering::Acquire).then_some(()));
        worker_gate.finished.store(true, Ordering::Release);
        worker_gate.wait.wake_all();
    }));
    if gate
        .wait
        .wait_until_timeout(
            || gate.started.load(Ordering::Acquire).then_some(()),
            TIMEOUT,
        )
        .is_err()
    {
        return Ok(false);
    }

    let single_backend = ShortReadBackend::new();
    let single = PageCache::new_unowned(None, Some(single_backend.clone()));
    let batch_backend = ShortReadBackend::new();
    let batch = PageCache::new_unowned(None, Some(batch_backend.clone()));
    let result = (|| {
        single.start_async_read(0)?;
        single.start_async_read(1)?;
        let single_good = single.inner.lock().get_entry(0).ok_or(SystemError::EIO)?;
        let single_error = single.inner.lock().get_entry(1).ok_or(SystemError::EIO)?;
        let completion = PageCacheReadBatchCompletion::reserve(&batch, 0, 2, None)?;
        // Pin identities before submit; error completion legitimately removes
        // a failed batch slot from the mapping.
        let batch_good = batch.inner.lock().get_entry(0).ok_or(SystemError::EIO)?;
        let batch_error = batch.inner.lock().get_entry(1).ok_or(SystemError::EIO)?;
        batch_backend.clone().submit_read_batch(
            PageCacheReadBatchRequest {
                start_index: 0,
                page_count: 2,
                valid_bytes: 2 * MMArch::PAGE_SIZE,
            },
            completion,
        );
        let ready = [&single_good, &single_error, &batch_good, &batch_error]
            .into_iter()
            .all(ready_before_release);
        Ok(ready
            && short_read_valid(&single_good)
            && short_read_valid(&batch_good)
            && single_error.state() == PageState::Error
            && batch_error.state() == PageState::Error
            && single_error
                .page
                .read()
                .flags()
                .contains(PageFlags::PG_ERROR)
            && batch_error
                .page
                .read()
                .flags()
                .contains(PageFlags::PG_ERROR)
            && single_backend
                .calls
                .iter()
                .all(|call| call.load(Ordering::Acquire) == 1)
            && batch_backend
                .calls
                .iter()
                .all(|call| call.load(Ordering::Acquire) == 1)
            && !gate.release.load(Ordering::Acquire))
    })();
    // Release even on failure before dropping caches: old kernels can still
    // have Loading work queued behind this gate. Never reproduce by hanging
    // the selftest process or leaving SYSTEM_WQ permanently occupied.
    drop(release);
    let released = gate
        .wait
        .wait_until_timeout(
            || gate.finished.load(Ordering::Acquire).then_some(()),
            TIMEOUT,
        )
        .is_ok();
    drop(single);
    drop(batch);
    result.map(|passed| passed && released)
}
