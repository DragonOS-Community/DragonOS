use super::{
    pc_stats, schedule_pagecache_io, Arc, AtomicBool, AtomicU64, AtomicUsize, Box, Duration,
    FilePrivateData, IndexNode, InodeRetentionGuard, InodeRetentionKind, MMArch,
    MemoryManagementArch, Mutex, Ordering, Page, PageCache, PageCacheDirtyCertificate,
    PageCacheManager, PageCacheWritebackProtocol, PageCacheWritebackProtocolState,
    PageCacheWritebackRange, PageEntry, PageFlags, PageIoWaiter, PageState, SystemError, Vec,
    VecDeque, WaitQueue, Weak, Work, WorkQueue, WritebackControl, PAGECACHE_IO_WORKERS,
};

static PAGE_CACHE_WRITEBACK_TAG_EPOCH: AtomicU64 = AtomicU64::new(1);
const MAX_ASYNC_WRITEBACK_BATCHES: usize = PAGECACHE_IO_WORKERS * 2;
static PAGECACHE_WRITEBACK_RR: AtomicUsize = AtomicUsize::new(0);
static ASYNC_WRITEBACK_BATCHES: AtomicUsize = AtomicUsize::new(0);
static ASYNC_WRITEBACK_COMPLETIONS: AtomicU64 = AtomicU64::new(0);
static ASYNC_WRITEBACK_WAIT: WaitQueue = WaitQueue::default();
static ASYNC_WRITEBACK_RETRIES: Mutex<VecDeque<Arc<AsyncWritebackRetryTicket>>> =
    Mutex::new(VecDeque::new());

lazy_static! {
    // Keep completion of already-published Writeback pages independent from
    // generic page-cache work. In particular, host invalidation runs on the
    // generic pool and may hold the filesystem admission barrier while waiting
    // for Writeback. Sharing a FIFO worker could strand the corresponding
    // writeback work behind that waiter and deadlock permanently.
    pub(super) static ref PAGECACHE_WRITEBACK_WQS: Vec<Arc<WorkQueue>> = {
        let mut wqs = Vec::new();
        for i in 0..PAGECACHE_IO_WORKERS {
            wqs.push(WorkQueue::new(&format!("pagecache-wb-{i}")));
        }
        wqs
    };
}

fn schedule_pagecache_writeback(work: Arc<Work>) {
    let idx =
        PAGECACHE_WRITEBACK_RR.fetch_add(1, Ordering::Relaxed) % PAGECACHE_WRITEBACK_WQS.len();
    PAGECACHE_WRITEBACK_WQS[idx].enqueue(work);
}

struct AsyncWritebackPermit;

impl AsyncWritebackPermit {
    /// Acquire a slot only when no tagged writeback retry is already queued.
    ///
    /// The retry queue lock serializes this decision with `Drop`: a release
    /// either transfers its slot to the FIFO head or makes it available here,
    /// never both.  This is required for `WAIT_AFTER` liveness: a new WRITE
    /// must not repeatedly overtake an older frozen generation.
    fn try_acquire() -> Option<Self> {
        let retries = ASYNC_WRITEBACK_RETRIES.lock();
        if !retries.is_empty() {
            return None;
        }
        Self::try_acquire_locked()
    }

    fn try_acquire_locked() -> Option<Self> {
        let mut current = ASYNC_WRITEBACK_BATCHES.load(Ordering::Acquire);
        loop {
            if current >= MAX_ASYNC_WRITEBACK_BATCHES {
                return None;
            }
            match ASYNC_WRITEBACK_BATCHES.compare_exchange_weak(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Some(Self),
                Err(observed) => current = observed,
            }
        }
    }

    /// Wait for production writeback to drain, then reserve the complete
    /// budget atomically with respect to normal acquisition and retry
    /// registration. The debug selftest needs exclusive ownership of every
    /// slot, but it may run immediately after another test has queued
    /// asynchronous writeback.
    fn acquire_all_for_selftest() -> Vec<Self> {
        ASYNC_WRITEBACK_WAIT.wait_until(|| {
            let retries = ASYNC_WRITEBACK_RETRIES.lock();
            if !retries.is_empty() || ASYNC_WRITEBACK_BATCHES.load(Ordering::Acquire) != 0 {
                return None;
            }

            let mut permits = Vec::with_capacity(MAX_ASYNC_WRITEBACK_BATCHES);
            for _ in 0..MAX_ASYNC_WRITEBACK_BATCHES {
                let Some(permit) = Self::try_acquire_locked() else {
                    unreachable!("idle writeback budget must have every slot available");
                };
                permits.push(permit);
            }
            Some(permits)
        })
    }

    /// Arrange a one-shot, non-blocking retry when a global batch slot is
    /// available.  A release transfers its permit directly to the FIFO head,
    /// so `sync_file_range(WRITE)` neither sleeps nor races queued waiters for
    /// a slot.
    fn register_retry(retry: Arc<AsyncWritebackRetryTicket>) {
        let permit = {
            let mut retries = ASYNC_WRITEBACK_RETRIES.lock();
            if retry.cancelled.load(Ordering::Acquire) {
                None
            } else if retries.is_empty() {
                match Self::try_acquire_locked() {
                    Some(permit) => Some(permit),
                    None => {
                        retries.push_back(retry.clone());
                        None
                    }
                }
            } else {
                retries.push_back(retry.clone());
                None
            }
        };
        if let Some(permit) = permit {
            retry.dispatch(permit);
        }
    }
}

impl Drop for AsyncWritebackPermit {
    fn drop(&mut self) {
        let retry = {
            let mut retries = ASYNC_WRITEBACK_RETRIES.lock();
            let mut next = None;
            while let Some(candidate) = retries.pop_front() {
                if !candidate.cancelled.load(Ordering::Acquire) {
                    next = Some(candidate);
                    break;
                }
            }
            if next.is_none() {
                let previous = ASYNC_WRITEBACK_BATCHES.fetch_sub(1, Ordering::AcqRel);
                debug_assert!(previous > 0, "async writeback permit underflow");
            }
            next
        };
        ASYNC_WRITEBACK_COMPLETIONS.fetch_add(1, Ordering::AcqRel);
        ASYNC_WRITEBACK_WAIT.wake_all();
        if let Some(retry) = retry {
            // Keep the counter unchanged: ownership of this exact slot moves
            // to the queued retry.  The callback must eventually consume or
            // drop this permit.
            retry.dispatch(AsyncWritebackPermit);
        }
    }
}

/// Exercise the global-budget scheduler without a filesystem backend.  The
/// fixture owns all slots, so retry registration must return immediately and
/// leave tickets queued; it verifies per-cache/epoch coalescing and Drop
/// cancellation, FIFO permit handoff, and that cancelled tickets never
/// receive a grant.
pub(super) fn run_async_writeback_budget_retry_selftest() -> bool {
    let mut permits = AsyncWritebackPermit::acquire_all_for_selftest();

    let cache = PageCache::new_unowned(None, None);
    let predecessor_page = match cache.get_or_create_page_zero(0) {
        Ok(page) => page,
        Err(_) => {
            drop(permits);
            return false;
        }
    };
    let tagged_page = match cache.get_or_create_page_zero(1) {
        Ok(page) => page,
        Err(_) => {
            drop(permits);
            return false;
        }
    };
    {
        let _tagged_writeback = cache.tagged_writeback_lock.lock();
        let mut inner = cache.inner.lock();
        let (Some(predecessor), Some(entry)) = (inner.get_entry(0), inner.get_entry(1)) else {
            drop(permits);
            return false;
        };
        predecessor.page.write().add_flags(PageFlags::PG_DIRTY);
        let predecessor_old_state = predecessor.state();
        predecessor.account_state_transition(predecessor_old_state, PageState::Dirty);
        predecessor.set_state(PageState::Dirty);
        predecessor.set_writeback_tag(0x69ff);
        inner.dirty_pages.insert(0);
        entry.page.write().add_flags(PageFlags::PG_DIRTY);
        let old_state = entry.state();
        entry.account_state_transition(old_state, PageState::Dirty);
        entry.set_state(PageState::Dirty);
        entry.set_writeback_tag(0x6a00);
        inner.dirty_pages.insert(1);
    }
    let coalesced =
        PageCacheManager::arm_tagged_writeback_budget_retry(&cache, None, 0, 2, 0x6a00, 2)
            .expect("first budget retry must create its ticket");
    let duplicate =
        PageCacheManager::arm_tagged_writeback_budget_retry(&cache, None, 0, 2, 0x6a00, 0)
            .is_none();
    AsyncWritebackPermit::register_retry(coalesced.clone());
    let coalesced_once = duplicate
        && cache.tagged_writeback_budget_retries.lock().len() == 1
        && cache
            .tagged_writeback_budget_retries
            .lock()
            .get(&0x6a00)
            .is_some_and(|retry| retry.cursor == 0);
    // Keep an older epoch in the same frozen range. A ticket belongs only to
    // its exact epoch: truncate must cancel 0x6a00 after removing page 1,
    // rather than treating the surviving 0x69ff tag as its work.
    let truncated = cache.truncate_locked(MMArch::PAGE_SIZE).ok() == Some(true);
    let exact_epoch_rejected =
        PageCacheManager::arm_tagged_writeback_budget_retry(&cache, None, 0, 2, 0x6a00, 0)
            .is_none();
    let truncate_cancelled = truncated
        && exact_epoch_rejected
        && coalesced.cancelled.load(Ordering::Acquire)
        && cache.tagged_writeback_budget_retries.lock().is_empty()
        && ASYNC_WRITEBACK_RETRIES.lock().is_empty();
    drop(predecessor_page);
    drop(tagged_page);
    drop(cache);

    let drop_cache = PageCache::new_unowned(None, None);
    let drop_page = match drop_cache.get_or_create_page_zero(0) {
        Ok(page) => page,
        Err(_) => {
            drop(permits);
            return false;
        }
    };
    {
        let _tagged_writeback = drop_cache.tagged_writeback_lock.lock();
        let mut inner = drop_cache.inner.lock();
        let Some(entry) = inner.get_entry(0) else {
            drop(permits);
            return false;
        };
        entry.page.write().add_flags(PageFlags::PG_DIRTY);
        let old_state = entry.state();
        entry.account_state_transition(old_state, PageState::Dirty);
        entry.set_state(PageState::Dirty);
        entry.set_writeback_tag(0x6a04);
        inner.dirty_pages.insert(0);
    }
    let drop_ticket =
        PageCacheManager::arm_tagged_writeback_budget_retry(&drop_cache, None, 0, 0, 0x6a04, 0)
            .expect("drop selftest must create its ticket");
    AsyncWritebackPermit::register_retry(drop_ticket.clone());
    drop(drop_page);
    drop(drop_cache);
    let cache_drop_cancelled =
        drop_ticket.cancelled.load(Ordering::Acquire) && ASYNC_WRITEBACK_RETRIES.lock().is_empty();

    let first = Arc::new(AsyncWritebackRetryTicket::new(Weak::new(), 0x6a01));
    let second = Arc::new(AsyncWritebackRetryTicket::new(Weak::new(), 0x6a02));
    AsyncWritebackPermit::register_retry(first.clone());
    AsyncWritebackPermit::register_retry(second.clone());
    let queued_fifo = {
        let retries = ASYNC_WRITEBACK_RETRIES.lock();
        retries.len() == 2
            && retries
                .front()
                .is_some_and(|ticket| Arc::ptr_eq(ticket, &first))
            && retries
                .back()
                .is_some_and(|ticket| Arc::ptr_eq(ticket, &second))
            && first.dispatches.load(Ordering::Acquire) == 0
            && second.dispatches.load(Ordering::Acquire) == 0
    };

    // The weak cache makes dispatch immediately return its transferred
    // permit. This deterministically exercises first -> second handoff with
    // no worker or backend timing dependency.
    drop(permits.pop());
    let fifo_handoff = first.dispatches.load(Ordering::Acquire) == 1
        && second.dispatches.load(Ordering::Acquire) == 1
        && ASYNC_WRITEBACK_RETRIES.lock().is_empty();
    drop(permits);

    let mut cancel_permits = Vec::with_capacity(MAX_ASYNC_WRITEBACK_BATCHES);
    for _ in 0..MAX_ASYNC_WRITEBACK_BATCHES {
        let Some(permit) = AsyncWritebackPermit::try_acquire() else {
            drop(cancel_permits);
            return false;
        };
        cancel_permits.push(permit);
    }
    let cancelled = Arc::new(AsyncWritebackRetryTicket::new(Weak::new(), 0x6a03));
    AsyncWritebackPermit::register_retry(cancelled.clone());
    cancelled.cancelled.store(true, Ordering::Release);
    drop(cancel_permits.pop());
    let cancelled_not_granted = cancelled.dispatches.load(Ordering::Acquire) == 0
        && ASYNC_WRITEBACK_RETRIES.lock().is_empty();
    drop(cancel_permits);

    coalesced_once
        && truncate_cancelled
        && cache_drop_cancelled
        && queued_fifo
        && fifo_handoff
        && cancelled_not_granted
        && ASYNC_WRITEBACK_BATCHES.load(Ordering::Acquire) == 0
}

/// Capture the completion generation before a reclaim scan schedules dirty
/// writeback.  Pair this with [`wait_for_async_writeback_progress`] so a fast
/// completion between the scan and the wait cannot be lost.
pub(crate) fn async_writeback_progress_snapshot() -> u64 {
    ASYNC_WRITEBACK_COMPLETIONS.load(Ordering::Acquire)
}

/// Throttle a no-progress reclaim pass until asynchronous writeback advances.
///
/// The wait is bounded: a stuck backend must not pin the global reclaimer
/// forever, because another cache may become reclaimable meanwhile.  Returning
/// `false` means no blocking wait was performed (the sampled generation had
/// already advanced, no writeback remained in flight, or the wait was
/// interrupted); callers should apply a short retry backoff instead of
/// immediately rescanning the same LRU.
pub(crate) fn wait_for_async_writeback_progress(observed: u64) -> bool {
    if ASYNC_WRITEBACK_COMPLETIONS.load(Ordering::Acquire) != observed {
        return false;
    }
    if ASYNC_WRITEBACK_BATCHES.load(Ordering::Acquire) == 0 {
        return false;
    }

    const RECLAIM_WRITEBACK_WAIT: Duration = Duration::from_millis(10);
    let result = ASYNC_WRITEBACK_WAIT.wait_until_timeout(
        || {
            if ASYNC_WRITEBACK_COMPLETIONS.load(Ordering::Acquire) != observed
                || ASYNC_WRITEBACK_BATCHES.load(Ordering::Acquire) == 0
            {
                Some(())
            } else {
                None
            }
        },
        RECLAIM_WRITEBACK_WAIT,
    );
    !matches!(result, Err(SystemError::ERESTARTSYS))
}

struct ReclaimerRunnerGuard {
    cache: Arc<PageCache>,
}

impl ReclaimerRunnerGuard {
    fn try_acquire(cache: &Arc<PageCache>) -> Option<Self> {
        cache
            .reclaimer_writeback_active
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| Self {
                cache: cache.clone(),
            })
    }
}

impl Drop for ReclaimerRunnerGuard {
    fn drop(&mut self) {
        self.cache
            .reclaimer_writeback_active
            .store(false, Ordering::Release);
    }
}

/// Immutable description of one contiguous PageCache batch selected for
/// writeback.
///
/// PageCache constructs this only after it has validated every fallible
/// page-cache condition, but before it publishes any page as `Writeback`.
/// A filesystem-specific backend may use it to bind an opaque submission
/// token to the precise logical range.  `valid_bytes` is the stable-EOF
/// clipped payload length; it can be zero when an already-dirty page lies
/// wholly beyond the captured EOF.  In that case PageCache does not invoke
/// `bind_writeback_submission()`: it retires the page with an empty payload
/// and never permits a filesystem-private token to be created.
/// A non-zero `writeback_generation` is a mapping-local claim identity for a
/// Token-protocol descriptor. It is assigned before the binding hook and is
/// stable through the paired submit/cancel path. Legacy and zero-payload
/// descriptors carry zero because they never create a submission token; this
/// keeps the eager writeback hot path free of unnecessary generation work.
/// The identity does not by itself describe per-page redirty state; a
/// filesystem which owns a persistent delayed-allocation ticket still has to
/// bind that ticket to this generation (or a stricter certificate).
#[derive(Debug, Eq, PartialEq)]
pub struct PageCacheWritebackDescriptor {
    first_index: usize,
    last_index: usize,
    file_size: usize,
    valid_bytes: usize,
    writeback_generation: u64,
    /// One exact Dirty-incarnation certificate per page in a non-empty Token
    /// batch. Legacy and zero-payload descriptors keep this vector empty.
    dirty_certificates: Vec<PageCacheDirtyCertificate>,
}

impl PageCacheWritebackDescriptor {
    pub const fn first_index(&self) -> usize {
        self.first_index
    }

    pub const fn last_index(&self) -> usize {
        self.last_index
    }

    pub const fn file_size(&self) -> usize {
        self.file_size
    }

    pub const fn valid_bytes(&self) -> usize {
        self.valid_bytes
    }

    /// Mapping-local identity for this Token-protocol Dirty -> Writeback
    /// claim, or zero for Legacy/zero-payload descriptors.
    pub const fn writeback_generation(&self) -> u64 {
        self.writeback_generation
    }

    pub(crate) fn dirty_certificates(&self) -> &[PageCacheDirtyCertificate] {
        &self.dirty_certificates
    }

    pub(crate) fn dirty_certificate(&self) -> Option<PageCacheDirtyCertificate> {
        let [certificate] = self.dirty_certificates.as_slice() else {
            return None;
        };
        Some(*certificate)
    }
}

/// A backend-owned wait ticket for a writeback operation which deliberately
/// deferred instead of failing.
///
/// The backend must arrange a concrete producer before returning this ticket.
/// `wait_for_progress()` must return only after that producer has changed the
/// condition which made the batch ineligible; it is not a polling hint.  The
/// ticket owns the producer's sequence/predicate recheck so a wake before the
/// caller begins sleeping is still observed as progress.  The ticket may be
/// waited only after PageCache has released its invalidate,
/// admission, page and index locks.  This keeps a delayed-allocation mapper
/// from recreating the PageCache/MM lock inversions that writeback avoids.
#[derive(Clone, Debug)]
pub enum PageCacheWritebackProgressOutcome {
    /// The backend's predicate advanced; PageCache may revalidate and retry.
    Progress,
    /// The deferred queue head was invalidated by truncate/unlink teardown.
    /// PageCache revalidates from that head; it never assumes later frozen
    /// pages were also cancelled.
    Cancelled,
    /// The backend cannot safely retry. PageCache must report the terminal
    /// error rather than turning it into another Deferred loop.
    Failed(SystemError),
}

pub trait PageCacheWritebackProgress: Send + Sync {
    /// Arm the concrete producer after PageCache has released every
    /// invalidate/admission/page/index guard. Backends whose producer is
    /// already running may keep the default no-op implementation.
    fn arm(&self) {}

    fn wait_for_progress(&self) -> PageCacheWritebackProgressOutcome;

    /// Register a non-blocking continuation which must be invoked exactly
    /// once after this ticket reaches either producer progress or a terminal
    /// cancellation/poison transition. A terminal ticket must never later
    /// return Deferred for the same head: `Cancelled` makes that head absent
    /// or ineligible before the callback, while `Failed` carries the one
    /// reportable error. Registration and the producer's
    /// sequence publication must share one linearization point: a callback
    /// registered before it belongs to that transition's callback set, while
    /// a callback registered after it observes the new sequence and runs
    /// immediately.  The callback must not run while the backend still holds
    /// a lock needed by PageCache retry; PageCache always revalidates cache
    /// identity, epoch and queue head after it runs.
    ///
    /// Async writeback and reclaimer use this instead of parking a bounded
    /// PageCache worker on an unbounded filesystem condition.  Backends must
    /// treat truncate, unlink and poison as a terminal ticket transition and
    /// invoke the callback with its explicit outcome, even when no normal
    /// mapping progress is possible.
    fn register_retry(&self, retry: Arc<dyn Fn(PageCacheWritebackProgressOutcome) + Send + Sync>);
}

/// Claim-time outcome of `PageCacheBackend::bind_writeback_submission()`.
///
/// `Deferred` is intentionally decided before PageCache changes any page from
/// Dirty to Writeback.  It is used for queue followers: their pages remain
/// retryable and no later batch may overtake the blocked queue head.
///
/// `Legacy` is a whole-backend opt-out from this protocol.  A backend which
/// returns `Submission` or `Deferred` for one non-empty descriptor must use
/// the token protocol for every non-empty descriptor in that mapping; mixing
/// `Legacy` with an ordered delayed-allocation queue would make it impossible
/// to preserve the queue-head ordering while retaining legacy parallelism.
pub enum PageCacheWritebackBindResult {
    /// Use the ordinary backend `write_pages()` path.
    Legacy,
    /// Own a precise filesystem-private claim through normal submission.
    Submission(Box<dyn PageCacheWritebackSubmission>),
    /// Leave every candidate Dirty and wait for the backend's producer.
    Deferred(Arc<dyn PageCacheWritebackProgress>),
}

/// Submission-time outcome of a previously bound writeback token.
///
/// Unlike `Err`, `Deferred` is not a writeback failure: PageCache returns the
/// batch to Dirty without setting PG_error or recording errseq, then lets a
/// synchronous caller wait for the supplied progress ticket before retrying.
pub enum PageCacheWritebackSubmitResult {
    Completed,
    Deferred(Arc<dyn PageCacheWritebackProgress>),
}

/// PageCache lock context in which a bound submission token must release its
/// filesystem-private claim.  The context is explicit because a backend may
/// return an admission error only after its callback has published a batch.
/// A token must never infer that invalidate-read is still held from the fact
/// that backend admission is no longer held.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PageCacheWritebackCancellationContext {
    /// Snapshotting failed while the original backend admission remains held.
    BeforeSubmitWithAdmission,
    /// The split ext4-style path released backend admission but retains
    /// invalidate-read through cancellation.
    AfterAdmissionWithInvalidateRead,
    /// A legacy/within-admission backend returned an error after its callback
    /// succeeded. Both its admission and invalidate-read may already be
    /// released; this context must not reacquire locks based on the split-path
    /// contract.
    AfterAdmissionWithoutInvalidateRead,
}

/// Filesystem-owned continuation for a PageCache batch which needs a
/// writeback-time mapping or other submission work.
///
/// Binding happens while PageCache holds the backend's write admission and
/// before it changes page state from Dirty to Writeback.  `submit()` runs only
/// after the PageCache invalidate/admission guards have been dropped, so it
/// must not reacquire either of those guards.  It owns any filesystem-private
/// claim until it explicitly resolves that claim on every success or error
/// path.
///
/// This first-generation token contract is completion-oriented: returning
/// `Completed` lets PageCache clean the pages immediately.  It therefore
/// cannot represent a lower layer which has merely accepted asynchronous I/O
/// while completion remains outstanding.  Such a backend must keep using a
/// completion-preserving path until PageCache grows an explicit
/// `Submitted(completion_handle)` result; it must never return `Completed`
/// early and let `WAIT_AFTER` observe clean pages before the actual I/O.
///
/// `cancel()` receives the exact lock context. In particular, only
/// `AfterAdmissionWithInvalidateRead` may use the split ext4 finalizer which
/// reacquires `size -> io`; `AfterAdmissionWithoutInvalidateRead` must be
/// independently safe after both guards have gone away. Both paths are
/// infallible: an internal state mismatch is fail-stop, never a reason to
/// silently discard a reservation.
///
/// A `submit()` error is a terminal writeback failure: before returning it,
/// the token must have finalized its private claim because PageCache will
/// record errseq and re-dirty the pages.  `Deferred(ticket)` likewise requires
/// the token to resolve its private claim before returning, but it is an
/// ordinary retry rather than an error.  The ticket must satisfy
/// `PageCacheWritebackProgress`'s no-polling contract.
pub trait PageCacheWritebackSubmission: Send {
    fn submit(
        self: Box<Self>,
        descriptor: &PageCacheWritebackDescriptor,
        data: &[u8],
    ) -> Result<PageCacheWritebackSubmitResult, SystemError>;

    fn cancel(self: Box<Self>, context: PageCacheWritebackCancellationContext);
}

pub trait PageCacheBackend: Send + Sync + core::fmt::Debug {
    fn read_page(&self, index: usize, buf: &mut [u8]) -> Result<usize, SystemError>;
    fn write_page(&self, index: usize, buf: &[u8]) -> Result<usize, SystemError>;
    fn npages(&self) -> usize;

    /// Reserve one filesystem block before publishing a new cache page.
    fn reserve_page(&self) -> Result<(), SystemError> {
        Ok(())
    }

    /// Release the reservation owned by one removed cache page.
    fn release_page(&self) {}

    /// Maximum number of consecutive pages which are useful in one backend
    /// write request.  Backends are single-page by default.
    fn write_batch_pages(&self) -> Result<usize, SystemError> {
        Ok(1)
    }

    /// Maximum useful batch beginning at the already identified first Dirty
    /// page. PageCache invokes this without its inner lock and revalidates the
    /// first page afterwards, so an ordered backend may inspect private queue
    /// state without introducing a PageCache -> filesystem lock inversion.
    fn write_batch_pages_from(&self, _first_index: usize) -> Result<usize, SystemError> {
        self.write_batch_pages()
    }

    /// Write a stable, i_size-clipped snapshot beginning at `start_index`.
    /// All chunks except the final one are full pages.
    fn write_pages(&self, start_index: usize, data: &[u8]) -> Result<(), SystemError> {
        for (page_offset, chunk) in data.chunks(MMArch::PAGE_SIZE).enumerate() {
            let index = start_index
                .checked_add(page_offset)
                .ok_or(SystemError::EOVERFLOW)?;
            match self.write_page(index, chunk) {
                Ok(written) if written == chunk.len() => {}
                Ok(_) => return Err(SystemError::EIO),
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    /// Select the lock order used to run writeback admission.
    ///
    /// The default preserves the established backend contract, in which a
    /// filesystem-specific admission barrier is acquired before PageCache's
    /// invalidate read lock.  FUSE relies on that ordering when host
    /// invalidation holds its barrier write lock before taking
    /// `invalidate_write`.  A backend which needs to acquire inode size/I/O
    /// serialization after invalidation (the future ext4 delalloc backend)
    /// must opt in explicitly.
    fn writeback_admission_order(&self) -> PageCacheWritebackAdmissionOrder {
        PageCacheWritebackAdmissionOrder::AdmissionBeforeInvalidate
    }

    /// Select the snapshot scope after a successful claim/bind. Backends that
    /// choose `AfterAdmission` must also opt into
    /// `InvalidateBeforeAdmission`; otherwise PageCache rejects the
    /// incompatible lock contract before publishing Dirty -> Writeback.
    fn writeback_snapshot_phase(&self) -> PageCacheWritebackSnapshotPhase {
        PageCacheWritebackSnapshotPhase::WithinAdmission
    }

    /// Run the page-cache claim while the filesystem's write admission is
    /// held.  The relative position of the PageCache invalidate read lock is
    /// selected by `writeback_admission_order()`.
    fn with_write_admission(
        &self,
        claim: &mut dyn FnMut() -> Result<(), SystemError>,
    ) -> Result<(), SystemError> {
        claim()
    }

    /// Try-only counterpart of `with_write_admission`.  Returning `false`
    /// means no page state was changed and the candidate remains Dirty.
    fn try_with_write_admission(
        &self,
        claim: &mut dyn FnMut() -> Result<(), SystemError>,
    ) -> Result<bool, SystemError> {
        claim()?;
        Ok(true)
    }

    /// Read authoritative i_size while filesystem admission and the page
    /// cache invalidate read lock are both held.
    fn stable_writeback_size(&self, inode: &Arc<dyn IndexNode>) -> Result<usize, SystemError> {
        Ok(inode.metadata()?.size.max(0) as usize)
    }

    /// Try-only counterpart of `stable_writeback_size()` for the reclaimer
    /// path.  `None` means the backend could not obtain a stable EOF without
    /// waiting; PageCache leaves the candidate Dirty and treats this round as
    /// having made no progress.  Legacy backends retain their existing
    /// behavior by delegating to the blocking-size implementation, while a
    /// backend which opts into a nonblocking admission protocol must override
    /// this and never issue I/O or wait for a contended lock.
    fn try_stable_writeback_size(
        &self,
        inode: &Arc<dyn IndexNode>,
    ) -> Result<Option<usize>, SystemError> {
        self.stable_writeback_size(inode).map(Some)
    }

    /// Declare the stable writeback protocol for every non-empty descriptor
    /// in this mapping. PageCache locks this value before invoking
    /// `bind_writeback_submission()`, so an implementation cannot create a
    /// private token and only then discover that an earlier batch used the
    /// incompatible Legacy path.
    ///
    /// Existing generic, FUSE and eager ext4 backends remain `Legacy`.
    /// A delayed-allocation backend must return `Token` even when its current
    /// queue head will answer `Deferred` rather than `Submission`.
    fn writeback_submission_protocol(&self) -> PageCacheWritebackProtocol {
        PageCacheWritebackProtocol::Legacy
    }

    /// Optionally bind a filesystem-private submission token to a fully
    /// validated PageCache candidate.
    ///
    /// PageCache calls this while its inner lock and this backend's write
    /// admission are held, immediately before Dirty -> Writeback publication.
    /// Therefore an implementation must not wait, take PageCache locks, or
    /// reacquire its admission lock.  A returned error leaves all PageCache
    /// pages Dirty.  `Submission(token)` transfers the exact-once
    /// responsibility for the descriptor to that token; `Legacy` preserves
    /// the existing `write_pages()` path. The result must match
    /// `writeback_submission_protocol()`; PageCache rejects a mismatch before
    /// publishing any page state and cancels a mismatching Submission while
    /// the admission guard is still held. `Deferred(ticket)` must leave no
    /// tentative private claim behind and must already have armed the ticket's
    /// producer; it leaves every candidate Dirty so queue followers cannot
    /// overtake it. If this function returns `Err`, it must itself roll back
    /// every tentative filesystem-private claim before doing so: PageCache has
    /// no token with which to cancel it.
    /// PageCache deliberately skips this hook for a descriptor with zero
    /// valid bytes, so EOF-exterior dirty pages cannot acquire a mapping or
    /// reservation token.
    fn bind_writeback_submission(
        &self,
        _descriptor: &PageCacheWritebackDescriptor,
    ) -> Result<PageCacheWritebackBindResult, SystemError> {
        Ok(PageCacheWritebackBindResult::Legacy)
    }

    fn read_page_async(&self, index: usize, page: &Arc<Page>) -> Arc<PageIoWaiter> {
        let waiter = PageIoWaiter::new();
        let result = {
            let mut guard = page.write();
            let dst = unsafe { guard.as_slice_mut() };
            self.read_page(index, dst)
        };
        waiter.complete(result);
        waiter
    }

    fn write_page_async(&self, index: usize, page: &Arc<Page>, len: usize) -> Arc<PageIoWaiter> {
        let waiter = PageIoWaiter::new();
        let result = {
            let guard = page.read();
            let src = unsafe { guard.as_slice() };
            let write_len = core::cmp::min(len, src.len());
            self.write_page(index, &src[..write_len])
        };
        waiter.complete(result);
        waiter
    }
}

/// Fixed lock orders supported by PageCache writeback admission.
///
/// This is deliberately a two-value protocol rather than an arbitrary backend
/// callback: every page-cache claim site follows one of these audited orders.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PageCacheWritebackAdmissionOrder {
    /// Preserve the legacy order used by generic and FUSE backends.
    AdmissionBeforeInvalidate,
    /// For an inode backend whose admission acquires locks ordered after
    /// PageCache invalidation, such as ext4's future `size_lock -> io_lock`.
    InvalidateBeforeAdmission,
}

/// Selects whether PageCache snapshots claimed pages while the backend's
/// admission is still held.
///
/// Most backends retain the historical `WithinAdmission` behavior. A future
/// ext4 delayed-allocation backend needs `AfterAdmission`: its claim/bind
/// phase holds `size_lock -> io_lock`, but `snapshot_writeback_batch()` may
/// acquire an AddressSpace lock through `mkclean_page()`. PageCache keeps its
/// invalidate read lock in that mode while releasing the backend admission
/// before snapshotting, so truncate cannot pass the claimed batch in between.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageCacheWritebackSnapshotPhase {
    /// Preserve the existing backend admission and snapshot scope.
    WithinAdmission,
    /// Snapshot after backend admission is released, while invalidate-read is
    /// still held. Only valid with `InvalidateBeforeAdmission`.
    AfterAdmission,
}

#[derive(Debug)]
pub struct AsyncPageCacheBackend {
    inode: Weak<dyn IndexNode>,
}

impl AsyncPageCacheBackend {
    pub fn new(inode: Weak<dyn IndexNode>) -> Self {
        Self { inode }
    }
}

impl PageCacheBackend for AsyncPageCacheBackend {
    fn read_page(&self, index: usize, buf: &mut [u8]) -> Result<usize, SystemError> {
        let inode = self.inode.upgrade().ok_or(SystemError::EIO)?;
        inode.read_sync(index * MMArch::PAGE_SIZE, buf)
    }

    fn write_page(&self, index: usize, buf: &[u8]) -> Result<usize, SystemError> {
        let inode = self.inode.upgrade().ok_or(SystemError::EIO)?;
        inode.write_sync(index * MMArch::PAGE_SIZE, buf)
    }

    fn npages(&self) -> usize {
        let inode = match self.inode.upgrade() {
            Some(inode) => inode,
            None => return 0,
        };
        match inode.metadata() {
            Ok(metadata) => {
                let size = metadata.size.max(0) as usize;
                if size == 0 {
                    0
                } else {
                    (size + MMArch::PAGE_SIZE - 1) >> MMArch::PAGE_SHIFT
                }
            }
            Err(_) => 0,
        }
    }

    fn read_page_async(&self, index: usize, page: &Arc<Page>) -> Arc<PageIoWaiter> {
        let waiter = PageIoWaiter::new();
        let inode = self.inode.clone();
        let page = page.clone();
        let waiter_cb = waiter.clone();
        let work = Work::new(move || {
            let inode = match inode.upgrade() {
                Some(inode) => inode,
                None => {
                    waiter_cb.complete(Err(SystemError::EIO));
                    return;
                }
            };
            let mut guard = page.write();
            let dst = unsafe { guard.as_slice_mut() };
            let res = inode.read_sync(index * MMArch::PAGE_SIZE, dst);
            waiter_cb.complete(res);
        });
        schedule_pagecache_io(work);
        waiter
    }

    fn write_page_async(&self, index: usize, page: &Arc<Page>, len: usize) -> Arc<PageIoWaiter> {
        let waiter = PageIoWaiter::new();
        let inode = self.inode.clone();
        let page = page.clone();
        let waiter_cb = waiter.clone();
        let work = Work::new(move || {
            let inode = match inode.upgrade() {
                Some(inode) => inode,
                None => {
                    waiter_cb.complete(Err(SystemError::EIO));
                    return;
                }
            };
            let data = {
                let guard = page.read();
                let src = unsafe { guard.as_slice() };
                let write_len = core::cmp::min(len, src.len());
                src[..write_len].to_vec()
            };
            let res = inode.write_sync(index * MMArch::PAGE_SIZE, &data);
            waiter_cb.complete(res);
        });
        schedule_pagecache_writeback(work);
        waiter
    }
}

#[derive(Debug)]
pub(super) struct TaggedWritebackSubmission {
    epoch: u64,
    first_index: usize,
    last_index: usize,
}

#[derive(Clone, Copy, Debug)]
struct TaggedWritebackCursor {
    start_index: usize,
    frozen_end: usize,
    epoch: u64,
    cursor: usize,
}

#[derive(Debug)]
pub(super) struct TaggedWritebackIncarnationRetry {
    entry: Arc<PageEntry>,
    observed_incarnation: u64,
    inode: Weak<dyn IndexNode>,
    continuation: TaggedWritebackCursor,
}

#[derive(Debug)]
struct AsyncWritebackRetryTicket {
    cache: Weak<PageCache>,
    epoch: u64,
    cancelled: AtomicBool,
    dispatches: AtomicUsize,
}

impl AsyncWritebackRetryTicket {
    fn new(cache: Weak<PageCache>, epoch: u64) -> Self {
        Self {
            cache,
            epoch,
            cancelled: AtomicBool::new(false),
            dispatches: AtomicUsize::new(0),
        }
    }

    fn dispatch(self: &Arc<Self>, permit: AsyncWritebackPermit) {
        let previous = self.dispatches.fetch_add(1, Ordering::AcqRel);
        debug_assert_eq!(previous, 0, "async writeback retry ticket dispatched twice");
        if self.cancelled.load(Ordering::Acquire) {
            return;
        }
        let Some(cache) = self.cache.upgrade() else {
            return;
        };
        cache
            .manager
            .resume_tagged_writeback_budget_retry(&cache, self, permit);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TaggedWritebackBudgetRetryState {
    Queued,
    Granted,
}

#[derive(Debug)]
pub(super) struct TaggedWritebackBudgetRetry {
    ticket: Arc<AsyncWritebackRetryTicket>,
    inode: Option<Weak<dyn IndexNode>>,
    pub(super) start_index: usize,
    pub(super) frozen_end: usize,
    cursor: usize,
    state: TaggedWritebackBudgetRetryState,
}

impl Drop for PageCache {
    fn drop(&mut self) {
        let tickets = {
            let mut pending = self.tagged_writeback_budget_retries.lock();
            pending
                .drain()
                .map(|(_, retry)| {
                    retry.ticket.cancelled.store(true, Ordering::Release);
                    retry.ticket
                })
                .collect::<Vec<_>>()
        };
        if tickets.is_empty() {
            return;
        }
        let mut retries = ASYNC_WRITEBACK_RETRIES.lock();
        retries.retain(|queued| !tickets.iter().any(|ticket| Arc::ptr_eq(ticket, queued)));
    }
}

/// RAII guard: ensures that a page entering Writeback state always calls
/// `finish_writeback_entry` on any early-exit path, preventing pages from
/// permanently stuck in Writeback.
struct WritebackGuard {
    cache: Arc<PageCache>,
    page_index: usize,
    entry: Arc<PageEntry>,
    page: Arc<Page>,
    disarmed: bool,
}

impl WritebackGuard {
    fn new(
        cache: Arc<PageCache>,
        page_index: usize,
        entry: Arc<PageEntry>,
        page: Arc<Page>,
    ) -> Self {
        Self {
            cache,
            page_index,
            entry,
            page,
            disarmed: false,
        }
    }

    /// Called on successful writeback completion to prevent Drop from re-processing.
    fn disarm(&mut self) {
        self.disarmed = true;
    }
}

impl Drop for WritebackGuard {
    fn drop(&mut self) {
        if !self.disarmed {
            // Page stuck in Writeback due to unexpected error; revert to Dirty for retry.
            let _ = PageCacheManager::finish_writeback_entry(
                self.cache.clone(),
                self.page_index,
                self.entry.clone(),
                self.page.clone(),
                Err(SystemError::EIO),
            );
        }
    }
}

pub(super) struct ClaimedWritebackBatch {
    cache: Arc<PageCache>,
    _domain_io: Option<super::PageCacheDomainIoPermit>,
    backend: Option<Arc<dyn PageCacheBackend>>,
    first_index: usize,
    pub(super) descriptor: PageCacheWritebackDescriptor,
    submission: Option<Box<dyn PageCacheWritebackSubmission>>,
    retry_writeback_tag: Option<u64>,
    writeback_incarnation: u64,
    entries: Vec<(usize, Arc<PageEntry>, Arc<Page>)>,
    guards: Vec<WritebackGuard>,
    data: Vec<u8>,
}

#[derive(Clone, Copy)]
pub(super) struct WritebackBatchRange {
    start_index: usize,
    end_index: usize,
}

impl WritebackBatchRange {
    pub(super) const fn new(start_index: usize, end_index: usize) -> Self {
        Self {
            start_index,
            end_index,
        }
    }
}

/// Internal result of inspecting one dirty run.  A deferred claim has not
/// altered any page state, while a deferred submission has already restored
/// its claimed batch to Dirty.
///
/// Keep the claimed batch inline. It is consumed immediately and already
/// owns the buffers used by writeback; boxing it would add an allocation to
/// every actual I/O batch and deepen the PageCache/workqueue auto-trait cycle.
#[allow(clippy::large_enum_variant)]
pub(super) enum WritebackClaimOutcome {
    NoBatch,
    Claimed(ClaimedWritebackBatch),
    Deferred(Arc<dyn PageCacheWritebackProgress>),
    /// A batch entered Writeback, then PageCache completed it back to Dirty
    /// and published this terminal error exactly once.  Callers which own a
    /// frozen tagged generation must retire its remaining tags without
    /// recording the error again.
    FailedRecorded(SystemError),
}

pub(super) enum WritebackSubmitOutcome {
    Completed,
    Failed(SystemError),
    Deferred(Arc<dyn PageCacheWritebackProgress>),
}

enum WritebackNextBatchOutcome {
    NoBatch,
    Completed,
    Deferred(Arc<dyn PageCacheWritebackProgress>),
}

/// Result of one non-blocking filesystem-driven writeback attempt.
///
/// This deliberately does not expose the deferred ticket: the filesystem
/// progress producer already owns the obligation which caused the attempt and
/// must reschedule itself without ever waiting on that same ticket.
pub(crate) enum PageCacheWritebackDispatchOutcome {
    Idle,
    Progress,
    Deferred,
}

enum TaggedWritebackSearch {
    Done,
    Advance(usize),
    Target {
        index: usize,
        entry: Arc<PageEntry>,
        end: usize,
    },
}

impl PageCacheWritebackRange {
    /// Wait until every page frozen by this range has returned from its
    /// backend writeout start path or has terminally retired. This is Linux's
    /// `WAIT_BEFORE|WRITE` boundary: it does not add a second wait for an
    /// already-published Writeback request, although a Legacy backend whose
    /// `write_pages()` itself is synchronous can make its start path wait.
    pub fn wait_for_submission(&self) -> Result<(), SystemError> {
        let cache = self.cache.upgrade().ok_or(SystemError::EIO)?;
        PageCacheManager::wait_tagged_writeback_submission(
            &cache,
            self.start_index,
            self.frozen_end
                .unwrap_or(self.start_index.saturating_sub(1)),
            self.epoch,
        )
    }

    /// Wait until every page tagged by this or an earlier still-pending range
    /// in the requested interval has either been submitted or terminally
    /// completed, and then wait for the resulting writeback I/O.
    pub fn wait_for_completion(&self) -> Result<(), SystemError> {
        let cache = self.cache.upgrade().ok_or(SystemError::EIO)?;
        PageCacheManager::wait_tagged_writeback_range(
            &cache,
            self.start_index,
            self.frozen_end,
            self.writeback_frontier,
            self.epoch,
        )
    }
}

impl PageCacheManager {
    fn lock_writeback_protocol(
        cache: &PageCache,
        protocol: PageCacheWritebackProtocol,
    ) -> Result<(), SystemError> {
        let expected = match protocol {
            PageCacheWritebackProtocol::Legacy => PageCacheWritebackProtocolState::Legacy as u8,
            PageCacheWritebackProtocol::Token => PageCacheWritebackProtocolState::Token as u8,
        };
        let mut observed = cache.writeback_protocol.load(Ordering::Acquire);
        loop {
            if observed == expected {
                return Ok(());
            }
            if observed != PageCacheWritebackProtocolState::Unset as u8 {
                // This must be checked before entering the backend bind hook.
                // A Submission result may already own a private reservation,
                // and turning that contract violation into an assert here
                // would leak it outside PageCache's cancellation boundary.
                return Err(SystemError::EIO);
            }
            match cache.writeback_protocol.compare_exchange_weak(
                observed,
                expected,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Ok(()),
                Err(current) => observed = current,
            }
        }
    }

    pub(super) fn claim_next_writeback_batch(
        cache: &Arc<PageCache>,
        start_index: usize,
        end_index: usize,
        file_size: usize,
        required_first: Option<(usize, &Arc<PageEntry>, u64)>,
        bind_submission: bool,
        admitted: Option<&super::PageCacheDomainIoPermit>,
    ) -> Result<WritebackClaimOutcome, SystemError> {
        let domain_io = match admitted {
            Some(permit) => Some(permit.derive()),
            None => cache.try_acquire_domain_io()?,
        };
        if start_index > end_index {
            return Ok(WritebackClaimOutcome::NoBatch);
        }
        // A tagged claim changes the wait predicate from
        // `Dirty(tagged)` to `Writeback(submission-record)`.  Serialize that
        // hand-off with the same lock used by waiters and generation tagging:
        // exposing a cleared tag before the record exists would let a
        // concurrent WAIT_BEFORE observe a false completed predicate.
        let _tagged_writeback_transition = required_first
            .as_ref()
            .map(|_| cache.tagged_writeback_lock.lock());
        let backend = cache.backend();
        if !bind_submission
            && (cache.writeback_protocol.load(Ordering::Acquire)
                == PageCacheWritebackProtocolState::Token as u8
                || backend.as_ref().is_some_and(|backend| {
                    backend.writeback_submission_protocol() == PageCacheWritebackProtocol::Token
                }))
        {
            // Launder/stable-size callers do not hold the token admission
            // contract. A mapping which has opted into (or declares) ordered
            // tokens must never silently fall back to write_pages() through
            // that side path, even before its first normal bind.
            return Err(SystemError::EIO);
        }
        // Locate only the prospective first page while holding PageCache
        // inner, then release it before asking an ordered backend for a
        // queue-aware bound. The full selector below revalidates this exact
        // identity, state and tag before publishing any Writeback state.
        let first_candidate = {
            let inner = cache.inner.lock();
            if let Some((required_index, required_entry, epoch)) = required_first {
                let Some(current) = inner.pages.get(&required_index) else {
                    return Ok(WritebackClaimOutcome::NoBatch);
                };
                if required_index < start_index
                    || required_index > end_index
                    || !Arc::ptr_eq(current, required_entry)
                    || !inner.dirty_pages.contains(&required_index)
                    || current.writeback_tag() != epoch
                    || !matches!(
                        current.state(),
                        PageState::UpToDate | PageState::Dirty | PageState::Error
                    )
                {
                    return Ok(WritebackClaimOutcome::NoBatch);
                }
                Some((required_index, current.clone()))
            } else {
                inner
                    .dirty_pages
                    .range(start_index..=end_index)
                    .find_map(|index| {
                        let entry = inner.pages.get(index)?;
                        (entry.writeback_tag() == 0
                            && matches!(
                                entry.state(),
                                PageState::UpToDate | PageState::Dirty | PageState::Error
                            ))
                        .then(|| (*index, entry.clone()))
                    })
            }
        };
        let Some((prospective_first_index, prospective_first_entry)) = first_candidate else {
            return Ok(WritebackClaimOutcome::NoBatch);
        };
        let reported_pages = match backend.as_ref() {
            Some(backend) => backend.write_batch_pages_from(prospective_first_index)?,
            None => 1,
        };
        if reported_pages == 0 {
            return Err(SystemError::EIO);
        }
        let batch_pages = reported_pages.min(64);
        let max_data_len = batch_pages
            .checked_mul(MMArch::PAGE_SIZE)
            .ok_or(SystemError::EOVERFLOW)?;
        let token_protocol = bind_submission
            && backend.as_ref().is_some_and(|backend| {
                backend.writeback_submission_protocol() == PageCacheWritebackProtocol::Token
            });

        let mut candidates = Vec::new();
        candidates
            .try_reserve_exact(batch_pages)
            .map_err(|_| SystemError::ENOMEM)?;
        let mut prepared = Vec::new();
        let mut guards = Vec::new();
        let mut data = Vec::new();
        let mut dirty_certificates = Vec::new();
        prepared
            .try_reserve_exact(batch_pages)
            .map_err(|_| SystemError::ENOMEM)?;
        guards
            .try_reserve_exact(batch_pages)
            .map_err(|_| SystemError::ENOMEM)?;
        data.try_reserve_exact(max_data_len)
            .map_err(|_| SystemError::ENOMEM)?;
        if token_protocol {
            dirty_certificates
                .try_reserve_exact(batch_pages)
                .map_err(|_| SystemError::ENOMEM)?;
        }

        let (first_index, descriptor, submission, writeback_incarnation) = {
            let mut inner = cache.inner.lock();
            if let Some((required_index, required_entry, epoch)) = required_first {
                let Some(current) = inner.pages.get(&required_index) else {
                    return Ok(WritebackClaimOutcome::NoBatch);
                };
                if required_index < start_index
                    || required_index > end_index
                    || !Arc::ptr_eq(current, required_entry)
                    || !inner.dirty_pages.contains(&required_index)
                    || current.writeback_tag() != epoch
                    || !matches!(
                        current.state(),
                        PageState::UpToDate | PageState::Dirty | PageState::Error
                    )
                {
                    return Ok(WritebackClaimOutcome::NoBatch);
                }
            }
            let mut expected = None;
            for index in inner.dirty_pages.range(start_index..=end_index) {
                if candidates.len() == batch_pages {
                    break;
                }
                let Some(entry) = inner.pages.get(index).cloned() else {
                    if candidates.is_empty() {
                        continue;
                    }
                    break;
                };
                let eligible = matches!(
                    entry.state(),
                    PageState::UpToDate | PageState::Dirty | PageState::Error
                );
                let tagged = required_first
                    .map(|(_, _, epoch)| entry.writeback_tag() == epoch)
                    .unwrap_or_else(|| entry.writeback_tag() == 0);
                if !eligible || !tagged {
                    if candidates.is_empty() {
                        continue;
                    }
                    break;
                }
                if let Some(expected) = expected {
                    if *index != expected {
                        break;
                    }
                }
                candidates.push((*index, entry));
                expected = index.checked_add(1);
            }
            let Some((first_index, _)) = candidates.first() else {
                return Ok(WritebackClaimOutcome::NoBatch);
            };
            let first_index = *first_index;
            if first_index != prospective_first_index
                || !Arc::ptr_eq(&candidates[0].1, &prospective_first_entry)
            {
                return Ok(WritebackClaimOutcome::NoBatch);
            }

            // Validate every fallible condition before publishing any member
            // as Writeback.  The state/identity recheck and dirty-set removal
            // below then happen under this same inner critical section.
            for (page_index, entry) in candidates.iter() {
                page_index
                    .checked_mul(MMArch::PAGE_SIZE)
                    .ok_or(SystemError::EOVERFLOW)?;
                let Some(current) = inner.pages.get(page_index) else {
                    return Ok(WritebackClaimOutcome::NoBatch);
                };
                if !Arc::ptr_eq(current, entry) {
                    return Ok(WritebackClaimOutcome::NoBatch);
                }
                if current.state() == PageState::Error {
                    return Err(SystemError::EIO);
                }
            }

            let last_index = candidates
                .last()
                .map(|(index, _)| *index)
                .expect("non-empty writeback candidate lost its tail");
            let first_offset = first_index
                .checked_mul(MMArch::PAGE_SIZE)
                .ok_or(SystemError::EOVERFLOW)?;
            let end_offset = last_index
                .checked_add(1)
                .and_then(|index| index.checked_mul(MMArch::PAGE_SIZE))
                .ok_or(SystemError::EOVERFLOW)?;
            let covered_bytes = end_offset
                .checked_sub(first_offset)
                .ok_or(SystemError::EOVERFLOW)?;
            let valid_bytes = file_size.saturating_sub(first_offset).min(covered_bytes);
            // Only a non-empty Token-protocol descriptor needs a generation:
            // generic/FUSE/eager-ext4 Legacy writeback neither binds nor
            // carries a submission token. The protocol is locked before the
            // generation is allocated so a backend cannot make this decision
            // after it has observed a descriptor.
            let token_protocol = match (bind_submission, backend.as_ref(), valid_bytes) {
                (true, Some(backend), valid_bytes) if valid_bytes != 0 => {
                    let protocol = backend.writeback_submission_protocol();
                    Self::lock_writeback_protocol(cache, protocol)?;
                    Some(protocol)
                }
                _ => None,
            };
            let writeback_generation = if token_protocol == Some(PageCacheWritebackProtocol::Token)
            {
                inner.allocate_writeback_generation()?
            } else {
                0
            };
            if token_protocol == Some(PageCacheWritebackProtocol::Token) {
                for (page_index, entry) in candidates.iter() {
                    dirty_certificates
                        .push(entry.current_dirty_certificate(cache.instance_id, *page_index)?);
                }
            }
            let claim_descriptor = PageCacheWritebackDescriptor {
                first_index,
                last_index,
                file_size,
                valid_bytes,
                writeback_generation,
                dirty_certificates,
            };

            // The backend's binding hook runs only after every PageCache
            // fallible precondition above has succeeded, but before any
            // Dirty page is published as Writeback.  Backends which return a
            // token have already entered write admission through the caller;
            // they may update their own small in-memory state here but must
            // not wait for PageCache or admission locks.
            let claim_submission = match (bind_submission, backend.as_ref()) {
                // A page wholly beyond the stable EOF still needs the normal
                // Dirty -> Writeback -> clean retirement, but it has no
                // payload to map or submit.  Do not delegate this case to a
                // filesystem hook: a buggy delayed-allocation backend must
                // not be able to manufacture a reservation token for it.
                (true, Some(_)) if claim_descriptor.valid_bytes() == 0 => None,
                (true, Some(backend)) => {
                    // The mode is a stable mapping capability, so check and
                    // lock it before the hook can manufacture a private
                    // token.  A backend which violates its own declaration
                    // is rejected as EIO while admission is still held.
                    let protocol = token_protocol
                        .expect("non-empty bind must have locked its writeback protocol");
                    match (
                        protocol,
                        backend.bind_writeback_submission(&claim_descriptor)?,
                    ) {
                        (
                            PageCacheWritebackProtocol::Legacy,
                            PageCacheWritebackBindResult::Legacy,
                        ) => None,
                        (
                            PageCacheWritebackProtocol::Token,
                            PageCacheWritebackBindResult::Submission(submission),
                        ) => Some(submission),
                        (
                            PageCacheWritebackProtocol::Token,
                            PageCacheWritebackBindResult::Deferred(progress),
                        ) => return Ok(WritebackClaimOutcome::Deferred(progress)),
                        (
                            PageCacheWritebackProtocol::Legacy,
                            PageCacheWritebackBindResult::Submission(submission),
                        ) => {
                            submission.cancel(
                                PageCacheWritebackCancellationContext::BeforeSubmitWithAdmission,
                            );
                            return Err(SystemError::EIO);
                        }
                        (
                            PageCacheWritebackProtocol::Legacy,
                            PageCacheWritebackBindResult::Deferred(_),
                        )
                        | (
                            PageCacheWritebackProtocol::Token,
                            PageCacheWritebackBindResult::Legacy,
                        ) => return Err(SystemError::EIO),
                    }
                }
                _ => None,
            };

            let writeback_incarnation = inner.allocate_writeback_incarnation()?;
            for (page_index, entry) in candidates.drain(..) {
                let old_state = entry.state();
                if old_state == PageState::UpToDate {
                    entry.account_state_transition(PageState::UpToDate, PageState::Dirty);
                    entry.set_state(PageState::Dirty);
                }
                debug_assert_eq!(entry.state(), PageState::Dirty);
                entry
                    .writeback_incarnation
                    .store(writeback_incarnation, Ordering::Release);
                entry.set_state(PageState::Writeback);
                if required_first.is_some() {
                    entry.set_writeback_tag(0);
                }
                entry.account_state_transition(PageState::Dirty, PageState::Writeback);
                inner.dirty_pages.remove(&page_index);
                inner.writeback_pages.insert(page_index);
                let page = entry.page.clone();
                guards.push(WritebackGuard::new(
                    cache.clone(),
                    page_index,
                    entry.clone(),
                    page.clone(),
                ));
                prepared.push((page_index, entry, page));
            }
            (
                first_index,
                claim_descriptor,
                claim_submission,
                writeback_incarnation,
            )
        };

        let retry_writeback_tag = required_first.map(|(_, _, epoch)| epoch);
        if retry_writeback_tag.is_some() {
            // Publish the record before notifying the tag clear.  The
            // transition lock above is also held by the waiter predicate, so
            // it cannot observe a moment in which this tagged batch is
            // neither Dirty-tagged nor represented by a pending record.
            Self::begin_tagged_writeback_submission_locked(
                cache,
                retry_writeback_tag,
                first_index,
                descriptor.last_index(),
            );
            Self::notify_tagged_writeback_progress(cache);
        }
        Ok(WritebackClaimOutcome::Claimed(ClaimedWritebackBatch {
            cache: cache.clone(),
            _domain_io: domain_io,
            backend,
            first_index,
            descriptor,
            submission,
            retry_writeback_tag,
            writeback_incarnation,
            entries: prepared,
            guards,
            data,
        }))
    }

    pub(super) fn complete_writeback_batch(
        mut batch: ClaimedWritebackBatch,
        result: Result<(), SystemError>,
    ) -> Result<(), SystemError> {
        if let Err(error) = result.as_ref() {
            batch
                .cache
                .record_writeback_error_with_superblock(error.clone());
        }
        let mut first_error = result.as_ref().err().cloned();
        for (guard, (page_index, entry, page)) in
            batch.guards.iter_mut().zip(batch.entries.drain(..))
        {
            guard.disarm();
            if let Err(error) = Self::finish_writeback_entry_state(
                batch.cache.clone(),
                page_index,
                entry,
                page,
                result.clone(),
                false,
            ) {
                first_error.get_or_insert(error);
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    /// Return an intentionally deferred batch to the Dirty set without
    /// reporting a writeback error.  The submission token has already
    /// finalized its filesystem-private claim and supplied the progress ticket
    /// which permits a later retry.
    ///
    /// All page flags are restored before the single `inner` critical section
    /// publishes *any* member as Dirty.  This is important for a filesystem
    /// token whose descriptor names one queue-head segment: exposing a Dirty
    /// prefix while the suffix is still Writeback would let a concurrent
    /// claim split that segment.  The page -> inner lock order is the same as
    /// ordinary completion; the transition is merely batched between those
    /// two phases.
    fn defer_writeback_batch(mut batch: ClaimedWritebackBatch) {
        // Capture the batch identity being retired before publishing Dirty. A
        // successor claim may allocate a new incarnation immediately after
        // the inner lock is released, and retries for the old owner must not
        // be mistaken for that successor.
        let completed_incarnation = batch.writeback_incarnation;
        for (guard, (_page_index, _entry, page)) in
            batch.guards.iter_mut().zip(batch.entries.iter())
        {
            guard.disarm();
            // A defer has performed no successful I/O.  In particular, it
            // must not erase PG_ERROR left by an earlier terminal submission;
            // only a later successful completion may acknowledge that bit.
            page.write().add_flags(PageFlags::PG_DIRTY);
        }

        {
            let mut inner = batch.cache.inner.lock();
            for (page_index, entry, _page) in batch.entries.iter() {
                let attached = inner
                    .pages
                    .get(page_index)
                    .is_some_and(|current| Arc::ptr_eq(current, entry));
                if attached {
                    entry.account_state_transition(PageState::Writeback, PageState::Dirty);
                    inner.writeback_pages.remove(page_index);
                    inner.dirty_pages.insert(*page_index);
                    if let Some(tag) = batch.retry_writeback_tag {
                        entry.set_writeback_tag(tag);
                    }
                }
                entry.set_state(PageState::Dirty);
            }
        }

        for (_page_index, entry, _page) in batch.entries.drain(..) {
            entry.wait_queue.wake_all();
            Self::dispatch_writeback_incarnation_retries(
                &batch.cache,
                &entry,
                completed_incarnation,
            );
        }
    }

    /// Resolve a bound filesystem submission with its exact PageCache lock
    /// context. The context is selected by the caller's state machine, never
    /// inferred by a token from a generic "after admission" label.
    fn cancel_submission(
        batch: &mut ClaimedWritebackBatch,
        context: PageCacheWritebackCancellationContext,
    ) {
        if let Some(submission) = batch.submission.take() {
            submission.cancel(context);
        }
    }

    pub(super) fn snapshot_writeback_batch(
        batch: &mut ClaimedWritebackBatch,
    ) -> Result<(), SystemError> {
        for (page_index, entry, page) in batch.entries.iter() {
            let page_start = page_index
                .checked_mul(MMArch::PAGE_SIZE)
                .ok_or(SystemError::EOVERFLOW)?;
            let len = batch
                .descriptor
                .file_size()
                .saturating_sub(page_start)
                .min(MMArch::PAGE_SIZE);
            // Every entry in this batch has already transitioned from Dirty
            // to Writeback.  Even when a concurrent size change leaves the
            // page wholly beyond the stable EOF, complete the dirty snapshot
            // transition so completion can retire it instead of observing
            // PG_DIRTY and requeueing it forever.  A zero length only omits
            // payload; it does not undo the claimed writeback state.
            batch.cache.mkclean_page(*page_index, false)?;
            let mut page_guard = page.write();
            // A front writer and this clear-for-I/O transition both hold the
            // exact page lock.  Recheck dirty-set membership under `inner`
            // before clearing: if a writer already published a successor
            // incarnation after this batch claimed the old one, its PG_DIRTY
            // must remain set for the next writeback scan.
            let has_successor = {
                let inner = batch.cache.inner.lock();
                inner
                    .pages
                    .get(page_index)
                    .is_some_and(|current| Arc::ptr_eq(current, entry))
                    && inner.dirty_pages.contains(page_index)
            };
            if !has_successor {
                page_guard.remove_flags(PageFlags::PG_DIRTY);
            }
            if len == 0 {
                continue;
            }
            let src = unsafe { page_guard.as_slice() };
            batch.data.extend_from_slice(&src[..len]);
        }
        Ok(())
    }

    /// Submit an already snapshotted batch.  This function must never acquire
    /// invalidate/admission locks: async callers may run after a truncate has
    /// started waiting for the published Writeback entries.
    pub(super) fn submit_writeback_batch(
        mut batch: ClaimedWritebackBatch,
    ) -> Result<WritebackSubmitOutcome, SystemError> {
        let submission = batch.submission.take();
        let submission_cache = batch.cache.clone();
        let submission_epoch = batch.retry_writeback_tag;
        let submission_first = batch.first_index;
        let submission_last = batch.descriptor.last_index();
        let result = if let Some(submission) = submission {
            assert_eq!(
                batch.data.len(),
                batch.descriptor.valid_bytes(),
                "writeback snapshot violated its stable descriptor"
            );
            submission.submit(&batch.descriptor, &batch.data)
        } else if batch.data.is_empty() {
            Ok(PageCacheWritebackSubmitResult::Completed)
        } else if let Some(backend) = batch.backend.as_ref() {
            backend
                .write_pages(batch.first_index, &batch.data)
                .map(|_| PageCacheWritebackSubmitResult::Completed)
        } else {
            let inode = batch
                .cache
                .inode()
                .and_then(|inode| inode.upgrade())
                .ok_or(SystemError::EIO);
            let offset = batch
                .first_index
                .checked_mul(MMArch::PAGE_SIZE)
                .ok_or(SystemError::EOVERFLOW);
            match (inode, offset) {
                (Ok(inode), Ok(offset)) => inode
                    .write_direct(
                        offset,
                        batch.data.len(),
                        &batch.data,
                        Mutex::new(FilePrivateData::Unused).lock(),
                    )
                    .and_then(|written| {
                        if written == batch.data.len() {
                            Ok(())
                        } else {
                            Err(SystemError::EIO)
                        }
                    })
                    .map(|_| PageCacheWritebackSubmitResult::Completed),
                (Err(error), _) | (_, Err(error)) => Err(error),
            }
        };
        match result {
            Ok(PageCacheWritebackSubmitResult::Completed) => {
                let completion = Self::complete_writeback_batch(batch, Ok(()));
                // A legacy write_pages() reports a synchronous start error
                // only on return. Keep the record through completion so
                // WAIT_BEFORE|WRITE observes the result it asked us to start.
                // Plain WRITE still returns promptly because this entire
                // path runs in a worker.
                Self::finish_tagged_writeback_submission(
                    &submission_cache,
                    submission_epoch,
                    submission_first,
                    submission_last,
                );
                completion?;
                Ok(WritebackSubmitOutcome::Completed)
            }
            Ok(PageCacheWritebackSubmitResult::Deferred(progress)) => {
                Self::defer_writeback_batch(batch);
                Self::finish_tagged_writeback_submission(
                    &submission_cache,
                    submission_epoch,
                    submission_first,
                    submission_last,
                );
                Ok(WritebackSubmitOutcome::Deferred(progress))
            }
            Err(error) => {
                // Completion deliberately reports the supplied terminal
                // error after it has restored every page to Dirty. That
                // return value is not a second failure of cleanup: consume
                // it here so callers can distinguish one recorded backend
                // error (`Failed`) from an internal PageCache failure.
                let completion = Self::complete_writeback_batch(batch, Err(error.clone()));
                Self::finish_tagged_writeback_submission(
                    &submission_cache,
                    submission_epoch,
                    submission_first,
                    submission_last,
                );
                debug_assert!(completion.is_err());
                Ok(WritebackSubmitOutcome::Failed(error))
            }
        }
    }

    fn claim_and_snapshot_with_stable_size(
        cache: &Arc<PageCache>,
        start_index: usize,
        end_index: usize,
        file_size: usize,
    ) -> Result<WritebackClaimOutcome, SystemError> {
        let _invalidate = cache.invalidate_read();
        Self::claim_and_snapshot_locked(cache, start_index, end_index, file_size, false)
    }

    /// Complete a claimed batch which has failed after Dirty -> Writeback was
    /// already published.  The terminal error is recorded exactly once here;
    /// callers receive `FailedRecorded` so tagged writeback can retire its
    /// generation without treating this as a bind-before-publication error.
    fn fail_claimed_batch_with_recorded_error(
        mut batch: ClaimedWritebackBatch,
        error: SystemError,
        cancellation_context: PageCacheWritebackCancellationContext,
    ) -> WritebackClaimOutcome {
        let submission_cache = batch.cache.clone();
        let submission_epoch = batch.retry_writeback_tag;
        let submission_first = batch.first_index;
        let submission_last = batch.descriptor.last_index();
        Self::cancel_submission(&mut batch, cancellation_context);
        let completion = Self::complete_writeback_batch(batch, Err(error.clone()));
        // The record was installed together with the tag clear, before
        // snapshotting. Retire it after PageCache completion and errseq
        // publication so a waiter cannot be stranded on a batch that will
        // never reach a worker.
        Self::finish_tagged_writeback_submission(
            &submission_cache,
            submission_epoch,
            submission_first,
            submission_last,
        );
        match completion {
            Err(completion_error) => {
                debug_assert_eq!(completion_error, error);
            }
            Ok(()) => panic!("failed writeback batch completed without an error"),
        }
        WritebackClaimOutcome::FailedRecorded(error)
    }

    /// Turn a backend admission error into an explicit completed failure when
    /// its callback had already bound a token.  A backend may discover an
    /// unlock/final-validation error only after the callback returns; at this
    /// point backend admission is no longer held, so the token must use its
    /// post-admission finalizer rather than be dropped unresolved.
    fn fail_claim_after_admission_error(
        claim: WritebackClaimOutcome,
        error: SystemError,
        cancellation_context: PageCacheWritebackCancellationContext,
    ) -> Result<WritebackClaimOutcome, SystemError> {
        match claim {
            WritebackClaimOutcome::Claimed(batch) => Ok(
                Self::fail_claimed_batch_with_recorded_error(batch, error, cancellation_context),
            ),
            // No Dirty -> Writeback transition was published, so this stays
            // a normal admission/bind error for the caller to report.
            WritebackClaimOutcome::NoBatch | WritebackClaimOutcome::Deferred(_) => Err(error),
            WritebackClaimOutcome::FailedRecorded(recorded) => {
                // A snapshot failure was already fully completed and
                // recorded inside the callback. A later backend error must
                // not manufacture a second error event for that batch.
                Ok(WritebackClaimOutcome::FailedRecorded(recorded))
            }
        }
    }

    /// Run a snapshot action after a claim and retain its token cleanup as a
    /// PageCache-owned state transition. The explicit cancellation context is
    /// selected solely by the PageCache lock state, never by a token.
    fn snapshot_claimed_batch_with(
        claim: WritebackClaimOutcome,
        snapshot: impl FnOnce(&mut ClaimedWritebackBatch) -> Result<(), SystemError>,
        cancellation_context: PageCacheWritebackCancellationContext,
    ) -> Result<WritebackClaimOutcome, SystemError> {
        let mut batch = match claim {
            WritebackClaimOutcome::Claimed(batch) => batch,
            WritebackClaimOutcome::NoBatch => return Ok(WritebackClaimOutcome::NoBatch),
            WritebackClaimOutcome::Deferred(progress) => {
                return Ok(WritebackClaimOutcome::Deferred(progress));
            }
            WritebackClaimOutcome::FailedRecorded(error) => {
                return Ok(WritebackClaimOutcome::FailedRecorded(error));
            }
        };
        if let Err(error) = snapshot(&mut batch) {
            return Ok(Self::fail_claimed_batch_with_recorded_error(
                batch,
                error,
                cancellation_context,
            ));
        }
        Ok(WritebackClaimOutcome::Claimed(batch))
    }

    /// Claim a batch and run its snapshot action while the caller still owns
    /// the relevant invalidate/admission guards. Keeping the error cleanup
    /// here makes the token cancellation order part of the production state
    /// machine rather than an obligation of individual claim callers.
    pub(super) fn claim_and_snapshot_locked_with(
        cache: &Arc<PageCache>,
        range: WritebackBatchRange,
        file_size: usize,
        required_first: Option<(usize, &Arc<PageEntry>, u64)>,
        bind_submission: bool,
        admitted: Option<&super::PageCacheDomainIoPermit>,
        snapshot: impl FnOnce(&mut ClaimedWritebackBatch) -> Result<(), SystemError>,
    ) -> Result<WritebackClaimOutcome, SystemError> {
        let claim = Self::claim_next_writeback_batch(
            cache,
            range.start_index,
            range.end_index,
            file_size,
            required_first,
            bind_submission,
            admitted,
        )?;
        Self::snapshot_claimed_batch_with(
            claim,
            snapshot,
            PageCacheWritebackCancellationContext::BeforeSubmitWithAdmission,
        )
    }

    /// Claim and snapshot with the invalidate read lock already held.
    pub(super) fn claim_and_snapshot_locked(
        cache: &Arc<PageCache>,
        start_index: usize,
        end_index: usize,
        file_size: usize,
        bind_submission: bool,
    ) -> Result<WritebackClaimOutcome, SystemError> {
        Self::claim_and_snapshot_locked_with(
            cache,
            WritebackBatchRange::new(start_index, end_index),
            file_size,
            None,
            bind_submission,
            None,
            Self::snapshot_writeback_batch,
        )
    }

    fn claim_and_snapshot_tagged_locked(
        cache: &Arc<PageCache>,
        range: WritebackBatchRange,
        file_size: usize,
        required_entry: &Arc<PageEntry>,
        epoch: u64,
        bind_submission: bool,
        admitted: Option<&super::PageCacheDomainIoPermit>,
    ) -> Result<WritebackClaimOutcome, SystemError> {
        Self::claim_and_snapshot_locked_with(
            cache,
            range,
            file_size,
            Some((range.start_index, required_entry, epoch)),
            bind_submission,
            admitted,
            Self::snapshot_writeback_batch,
        )
    }

    fn writeback_next_batch_with_stable_size(
        cache: &Arc<PageCache>,
        start_index: usize,
        end_index: usize,
        file_size: usize,
    ) -> Result<bool, SystemError> {
        let claim =
            Self::claim_and_snapshot_with_stable_size(cache, start_index, end_index, file_size)?;
        let batch = match claim {
            WritebackClaimOutcome::Claimed(batch) => batch,
            WritebackClaimOutcome::NoBatch => return Ok(false),
            WritebackClaimOutcome::Deferred(_) => {
                unreachable!("stable-size writeback must not bind a deferred submission")
            }
            WritebackClaimOutcome::FailedRecorded(error) => return Err(error),
        };
        match Self::submit_writeback_batch(batch)? {
            WritebackSubmitOutcome::Completed => Ok(true),
            WritebackSubmitOutcome::Failed(error) => Err(error),
            WritebackSubmitOutcome::Deferred(_) => {
                panic!("stable-size writeback must not submit a deferred token")
            }
        }
    }

    /// Run one page-cache writeback claim in the backend's audited admission
    /// order.  Submission deliberately runs after both guards have been
    /// dropped.
    pub(super) fn with_writeback_admission(
        cache: &Arc<PageCache>,
        backend: &Arc<dyn PageCacheBackend>,
        claim: &mut dyn FnMut() -> Result<(), SystemError>,
    ) -> Result<(), SystemError> {
        match backend.writeback_admission_order() {
            PageCacheWritebackAdmissionOrder::AdmissionBeforeInvalidate => backend
                .with_write_admission(&mut || {
                    let _invalidate = cache.invalidate_read();
                    claim()
                }),
            PageCacheWritebackAdmissionOrder::InvalidateBeforeAdmission => {
                let _invalidate = cache.invalidate_read();
                backend.with_write_admission(claim)
            }
        }
    }

    /// Try-only counterpart used by reclaimer workers.  It makes no page-state
    /// change unless both sides of the selected lock order have admitted the
    /// claim.
    pub(super) fn try_with_writeback_admission(
        cache: &Arc<PageCache>,
        backend: &Arc<dyn PageCacheBackend>,
        claim: &mut dyn FnMut() -> Result<(), SystemError>,
    ) -> Result<bool, SystemError> {
        match backend.writeback_admission_order() {
            PageCacheWritebackAdmissionOrder::AdmissionBeforeInvalidate => {
                let mut invalidate_acquired = false;
                let admitted = backend.try_with_write_admission(&mut || {
                    let Some(_invalidate) = cache.try_invalidate_read() else {
                        return Ok(());
                    };
                    invalidate_acquired = true;
                    claim()
                })?;
                Ok(admitted && invalidate_acquired)
            }
            PageCacheWritebackAdmissionOrder::InvalidateBeforeAdmission => {
                let Some(_invalidate) = cache.try_invalidate_read() else {
                    return Ok(false);
                };
                backend.try_with_write_admission(claim)
            }
        }
    }

    /// Bind a candidate while the backend admission is held, then snapshot it
    /// after that admission has been released but before invalidate-read is
    /// released. This is intentionally restricted to the ext4-style lock
    /// order: changing the legacy/FUSE order would recreate its invalidation
    /// ABBA rather than solve the ext4 mmap one.
    pub(super) fn claim_and_snapshot_after_admission_with(
        cache: &Arc<PageCache>,
        backend: &Arc<dyn PageCacheBackend>,
        range: WritebackBatchRange,
        required_first: Option<(usize, &Arc<PageEntry>, u64)>,
        admitted: Option<&super::PageCacheDomainIoPermit>,
        mut stable_size: impl FnMut() -> Result<usize, SystemError>,
        snapshot: impl FnOnce(&mut ClaimedWritebackBatch) -> Result<(), SystemError>,
    ) -> Result<WritebackClaimOutcome, SystemError> {
        debug_assert_eq!(
            backend.writeback_admission_order(),
            PageCacheWritebackAdmissionOrder::InvalidateBeforeAdmission
        );
        let _invalidate = cache.invalidate_read();
        let mut claim = WritebackClaimOutcome::NoBatch;
        let admission = backend.with_write_admission(&mut || {
            let file_size = stable_size()?;
            claim = Self::claim_next_writeback_batch(
                cache,
                range.start_index,
                range.end_index,
                file_size,
                required_first,
                true,
                admitted,
            )?;
            Ok(())
        });
        if let Err(error) = admission {
            return Self::fail_claim_after_admission_error(
                claim,
                error,
                PageCacheWritebackCancellationContext::AfterAdmissionWithInvalidateRead,
            );
        }
        Self::snapshot_claimed_batch_with(
            claim,
            snapshot,
            PageCacheWritebackCancellationContext::AfterAdmissionWithInvalidateRead,
        )
    }

    /// Try-only variant of `claim_and_snapshot_after_admission_with()`. No
    /// state changes when invalidate-read, backend admission, or stable EOF
    /// cannot be obtained immediately.
    pub(super) fn try_claim_and_snapshot_after_admission_with(
        cache: &Arc<PageCache>,
        backend: &Arc<dyn PageCacheBackend>,
        start_index: usize,
        end_index: usize,
        admitted: Option<&super::PageCacheDomainIoPermit>,
        mut stable_size: impl FnMut() -> Result<Option<usize>, SystemError>,
        snapshot: impl FnOnce(&mut ClaimedWritebackBatch) -> Result<(), SystemError>,
    ) -> Result<WritebackClaimOutcome, SystemError> {
        debug_assert_eq!(
            backend.writeback_admission_order(),
            PageCacheWritebackAdmissionOrder::InvalidateBeforeAdmission
        );
        let Some(_invalidate) = cache.try_invalidate_read() else {
            return Ok(WritebackClaimOutcome::NoBatch);
        };
        let mut claim = WritebackClaimOutcome::NoBatch;
        let mut stable_size_available = false;
        let admitted = match backend.try_with_write_admission(&mut || {
            let Some(file_size) = stable_size()? else {
                return Ok(());
            };
            stable_size_available = true;
            claim = Self::claim_next_writeback_batch(
                cache,
                start_index,
                end_index,
                file_size,
                None,
                true,
                admitted,
            )?;
            Ok(())
        }) {
            Ok(admitted) => admitted,
            Err(error) => {
                return Self::fail_claim_after_admission_error(
                    claim,
                    error,
                    PageCacheWritebackCancellationContext::AfterAdmissionWithInvalidateRead,
                );
            }
        };
        if !admitted || !stable_size_available {
            return Ok(WritebackClaimOutcome::NoBatch);
        }
        Self::snapshot_claimed_batch_with(
            claim,
            snapshot,
            PageCacheWritebackCancellationContext::AfterAdmissionWithInvalidateRead,
        )
    }

    fn claim_next_batch_with_admission(
        &self,
        cache: &Arc<PageCache>,
        inode: &Arc<dyn IndexNode>,
        start_index: usize,
        end_index: usize,
        admitted: Option<&super::PageCacheDomainIoPermit>,
    ) -> Result<WritebackClaimOutcome, SystemError> {
        let Some(backend) = cache.backend() else {
            let _invalidate = cache.invalidate_read();
            let file_size = inode.metadata()?.size.max(0) as usize;
            return Self::claim_and_snapshot_locked_with(
                cache,
                WritebackBatchRange::new(start_index, end_index),
                file_size,
                None,
                false,
                admitted,
                Self::snapshot_writeback_batch,
            );
        };
        if backend.writeback_snapshot_phase() == PageCacheWritebackSnapshotPhase::AfterAdmission {
            if backend.writeback_admission_order()
                != PageCacheWritebackAdmissionOrder::InvalidateBeforeAdmission
            {
                return Err(SystemError::EINVAL);
            }
            return Self::claim_and_snapshot_after_admission_with(
                cache,
                &backend,
                WritebackBatchRange::new(start_index, end_index),
                None,
                admitted,
                || backend.stable_writeback_size(inode),
                Self::snapshot_writeback_batch,
            );
        }
        let mut claimed = WritebackClaimOutcome::NoBatch;
        let admission = Self::with_writeback_admission(cache, &backend, &mut || {
            let file_size = backend.stable_writeback_size(inode)?;
            claimed = Self::claim_and_snapshot_locked_with(
                cache,
                WritebackBatchRange::new(start_index, end_index),
                file_size,
                None,
                true,
                admitted,
                Self::snapshot_writeback_batch,
            )?;
            Ok(())
        });
        if let Err(error) = admission {
            return Self::fail_claim_after_admission_error(
                claimed,
                error,
                PageCacheWritebackCancellationContext::AfterAdmissionWithoutInvalidateRead,
            );
        }
        Ok(claimed)
    }

    fn claim_tagged_batch_with_admission(
        &self,
        cache: &Arc<PageCache>,
        inode: &Arc<dyn IndexNode>,
        range: WritebackBatchRange,
        required_entry: &Arc<PageEntry>,
        epoch: u64,
        admitted: Option<&super::PageCacheDomainIoPermit>,
    ) -> Result<WritebackClaimOutcome, SystemError> {
        let Some(backend) = cache.backend() else {
            let _invalidate = cache.invalidate_read();
            let file_size = inode.metadata()?.size.max(0) as usize;
            return Self::claim_and_snapshot_tagged_locked(
                cache,
                range,
                file_size,
                required_entry,
                epoch,
                false,
                admitted,
            );
        };
        if backend.writeback_snapshot_phase() == PageCacheWritebackSnapshotPhase::AfterAdmission {
            if backend.writeback_admission_order()
                != PageCacheWritebackAdmissionOrder::InvalidateBeforeAdmission
            {
                return Err(SystemError::EINVAL);
            }
            return Self::claim_and_snapshot_after_admission_with(
                cache,
                &backend,
                range,
                Some((range.start_index, required_entry, epoch)),
                admitted,
                || backend.stable_writeback_size(inode),
                Self::snapshot_writeback_batch,
            );
        }
        let mut claimed = WritebackClaimOutcome::NoBatch;
        let admission = Self::with_writeback_admission(cache, &backend, &mut || {
            let file_size = backend.stable_writeback_size(inode)?;
            claimed = Self::claim_and_snapshot_tagged_locked(
                cache,
                range,
                file_size,
                required_entry,
                epoch,
                true,
                admitted,
            )?;
            Ok(())
        });
        if let Err(error) = admission {
            return Self::fail_claim_after_admission_error(
                claimed,
                error,
                PageCacheWritebackCancellationContext::AfterAdmissionWithoutInvalidateRead,
            );
        }
        Ok(claimed)
    }

    fn try_claim_next_batch_with_admission(
        &self,
        cache: &Arc<PageCache>,
        inode: &Arc<dyn IndexNode>,
        start_index: usize,
        end_index: usize,
        admitted: Option<&super::PageCacheDomainIoPermit>,
    ) -> Result<WritebackClaimOutcome, SystemError> {
        let Some(backend) = cache.backend() else {
            let Some(_invalidate) = cache.try_invalidate_read() else {
                return Ok(WritebackClaimOutcome::NoBatch);
            };
            let file_size = inode.metadata()?.size.max(0) as usize;
            return Self::claim_and_snapshot_locked(
                cache,
                start_index,
                end_index,
                file_size,
                false,
            );
        };
        if backend.writeback_snapshot_phase() == PageCacheWritebackSnapshotPhase::AfterAdmission {
            if backend.writeback_admission_order()
                != PageCacheWritebackAdmissionOrder::InvalidateBeforeAdmission
            {
                return Err(SystemError::EINVAL);
            }
            return Self::try_claim_and_snapshot_after_admission_with(
                cache,
                &backend,
                start_index,
                end_index,
                admitted,
                || backend.try_stable_writeback_size(inode),
                Self::snapshot_writeback_batch,
            );
        }
        Self::try_claim_and_snapshot_within_admission_with_stable_size(
            cache,
            &backend,
            start_index,
            end_index,
            admitted,
            || backend.try_stable_writeback_size(inode),
        )
    }

    /// Try to claim one batch after a backend has supplied an immediately
    /// available stable EOF.  `None` is a normal reclaimer skip: the backend
    /// may have observed an inode lock or a cold metadata cache, and PageCache
    /// must leave every candidate Dirty without issuing I/O or waiting.
    pub(super) fn try_claim_and_snapshot_within_admission_with_stable_size(
        cache: &Arc<PageCache>,
        backend: &Arc<dyn PageCacheBackend>,
        start_index: usize,
        end_index: usize,
        admitted: Option<&super::PageCacheDomainIoPermit>,
        mut stable_size: impl FnMut() -> Result<Option<usize>, SystemError>,
    ) -> Result<WritebackClaimOutcome, SystemError> {
        // This helper is deliberately unavailable to split-phase backends:
        // calling it would silently move `mkclean_page()` back under
        // size/I/O admission and recreate the ext4 mmap lock hazard.
        if backend.writeback_snapshot_phase() != PageCacheWritebackSnapshotPhase::WithinAdmission {
            return Err(SystemError::EINVAL);
        }
        let mut claimed = WritebackClaimOutcome::NoBatch;
        let admitted = match Self::try_with_writeback_admission(cache, backend, &mut || {
            let Some(file_size) = stable_size()? else {
                return Ok(());
            };
            claimed = Self::claim_and_snapshot_locked_with(
                cache,
                WritebackBatchRange::new(start_index, end_index),
                file_size,
                None,
                true,
                admitted,
                Self::snapshot_writeback_batch,
            )?;
            Ok(())
        }) {
            Ok(admitted) => admitted,
            Err(error) => {
                return Self::fail_claim_after_admission_error(
                    claimed,
                    error,
                    PageCacheWritebackCancellationContext::AfterAdmissionWithoutInvalidateRead,
                );
            }
        };
        if !admitted {
            return Ok(WritebackClaimOutcome::NoBatch);
        }
        Ok(claimed)
    }

    fn writeback_next_batch(
        &self,
        cache: &Arc<PageCache>,
        inode: &Arc<dyn IndexNode>,
        start_index: usize,
        end_index: usize,
        admitted: Option<&super::PageCacheDomainIoPermit>,
    ) -> Result<WritebackNextBatchOutcome, SystemError> {
        match self.claim_next_batch_with_admission(
            cache,
            inode,
            start_index,
            end_index,
            admitted,
        )? {
            WritebackClaimOutcome::NoBatch => Ok(WritebackNextBatchOutcome::NoBatch),
            WritebackClaimOutcome::Deferred(progress) => {
                Ok(WritebackNextBatchOutcome::Deferred(progress))
            }
            WritebackClaimOutcome::FailedRecorded(error) => Err(error),
            WritebackClaimOutcome::Claimed(batch) => match Self::submit_writeback_batch(batch)? {
                WritebackSubmitOutcome::Completed => Ok(WritebackNextBatchOutcome::Completed),
                WritebackSubmitOutcome::Failed(error) => Err(error),
                WritebackSubmitOutcome::Deferred(progress) => {
                    Ok(WritebackNextBatchOutcome::Deferred(progress))
                }
            },
        }
    }

    fn wait_data_range_clean(
        cache: &Arc<PageCache>,
        start_index: usize,
        end_index: usize,
    ) -> Result<bool, SystemError> {
        if start_index > end_index {
            return Ok(true);
        }
        Self::wait_writeback_range_bounded(cache, start_index, end_index)?;
        let inner = cache.inner.lock();
        let has_dirty = inner
            .dirty_pages
            .range(start_index..=end_index)
            .next()
            .is_some();
        let has_writeback = inner
            .writeback_pages
            .range(start_index..=end_index)
            .next()
            .is_some();
        Ok(!has_dirty && !has_writeback)
    }

    fn wait_writeback_range_bounded(
        cache: &Arc<PageCache>,
        start_index: usize,
        end_index: usize,
    ) -> Result<(), SystemError> {
        Self::wait_writeback_range_bounded_through(cache, start_index, end_index, None)
    }

    /// Wait only for writeback incarnations which existed at a frozen
    /// boundary. A later redirty/claim at the same index has a larger
    /// incarnation and must not extend an older fsync.
    fn wait_writeback_range_bounded_through(
        cache: &Arc<PageCache>,
        start_index: usize,
        end_index: usize,
        frontier: Option<u64>,
    ) -> Result<(), SystemError> {
        if start_index > end_index {
            return Ok(());
        }
        const WAIT_BATCH_ENTRIES: usize = 64;
        const WAIT_SCAN_INDICES: usize = 256;
        let mut cursor = start_index;
        let mut first_error = None;
        loop {
            if cursor > end_index {
                break;
            }
            let (entries, last_scanned) = {
                let inner = cache.inner.lock();
                let mut entries = Vec::new();
                entries
                    .try_reserve_exact(WAIT_BATCH_ENTRIES)
                    .map_err(|_| SystemError::ENOMEM)?;
                let mut last_scanned = None;
                let mut scanned = 0usize;
                for index in inner.page_indices.range(cursor..=end_index) {
                    last_scanned = Some(*index);
                    scanned += 1;
                    let Some(entry) = inner.pages.get(index) else {
                        if scanned == WAIT_SCAN_INDICES {
                            break;
                        }
                        continue;
                    };
                    let incarnation = entry.writeback_incarnation.load(Ordering::Acquire);
                    if entry.state() == PageState::Writeback
                        && frontier.is_none_or(|frontier| incarnation <= frontier)
                    {
                        entries.push((entry.clone(), incarnation));
                    }
                    if entries.len() == WAIT_BATCH_ENTRIES || scanned == WAIT_SCAN_INDICES {
                        break;
                    }
                }
                (entries, last_scanned)
            };
            let Some(last_scanned) = last_scanned else {
                break;
            };
            for (entry, incarnation) in entries {
                if let Err(error) = Self::wait_writeback_entry_incarnation(entry, incarnation) {
                    first_error.get_or_insert(error);
                }
            }
            if last_scanned == usize::MAX {
                break;
            }
            cursor = last_scanned + 1;
            crate::sched::sched_yield();
        }
        first_error.map_or(Ok(()), Err)
    }

    fn sync_data_with_stable_size(
        &self,
        start_index: usize,
        end_index: usize,
        file_size: usize,
    ) -> Result<(), SystemError> {
        let cache = self.upgrade()?;
        loop {
            while Self::writeback_next_batch_with_stable_size(
                &cache,
                start_index,
                end_index,
                file_size,
            )? {}
            if Self::wait_data_range_clean(&cache, start_index, end_index)? {
                return Ok(());
            }
        }
    }

    fn sync_data_admitted(
        &self,
        start_index: usize,
        end_index: usize,
        file_size: usize,
    ) -> Result<(), SystemError> {
        self.sync_data_with_stable_size(start_index, end_index, file_size)
    }

    fn sync_data(&self, start_index: usize, end_index: usize) -> Result<(), SystemError> {
        let cache = self.upgrade()?;
        let inode = cache
            .inode()
            .and_then(|inode| inode.upgrade())
            .ok_or(SystemError::EIO)?;
        loop {
            loop {
                match self.writeback_next_batch(&cache, &inode, start_index, end_index, None)? {
                    WritebackNextBatchOutcome::NoBatch => break,
                    WritebackNextBatchOutcome::Completed => continue,
                    WritebackNextBatchOutcome::Deferred(progress) => {
                        // `writeback_next_batch()` has completed the token
                        // transition and released all PageCache/admission
                        // guards.  Waiting here therefore cannot block a
                        // mapper which needs any of those locks to make its
                        // promised progress.
                        progress.arm();
                        if let PageCacheWritebackProgressOutcome::Failed(error) =
                            progress.wait_for_progress()
                        {
                            return Err(error);
                        }
                        break;
                    }
                }
            }
            if Self::wait_data_range_clean(&cache, start_index, end_index)? {
                return Ok(());
            }
        }
    }

    pub fn sync(&self) -> Result<(), SystemError> {
        let cache = self.upgrade()?;
        // Keep the canonical inode alive across the boundary between the last
        // page completing writeback and write_inode(). The aggregated dirty
        // owner may be released by finish_writeback_entry(), but eviction must
        // not enter that false-zero window before metadata is committed.
        let Some(sync_inode) = cache.inode().and_then(|inode| inode.upgrade()) else {
            cache.record_writeback_error_with_superblock(SystemError::EIO);
            return Err(SystemError::EIO);
        };
        let _sync_retention =
            match InodeRetentionGuard::new(sync_inode.clone(), InodeRetentionKind::AsyncWork) {
                Ok(guard) => guard,
                Err(error) => {
                    cache.record_writeback_error_with_superblock(error.clone());
                    return Err(error);
                }
            };
        self.sync_data(0, usize::MAX)?;

        // 脏页写完后调 write_inode 回写元数据。
        let wbc = WritebackControl::sync_all_for_sync();
        if let Err(e) = sync_inode.write_inode(&wbc) {
            log::warn!("write_inode failed: {:?}", e);
            cache.record_writeback_error_with_superblock(e.clone());
            return Err(e);
        }

        Ok(())
    }

    /// Synchronize after the filesystem has already blocked new dirty-page
    /// admission and supplied the authoritative i_size.  Callers must not use
    /// this as a substitute for `PageCacheBackend::with_write_admission`.
    pub(crate) fn sync_with_stable_size(&self, file_size: usize) -> Result<(), SystemError> {
        let cache = self.upgrade()?;
        let Some(sync_inode) = cache.inode().and_then(|inode| inode.upgrade()) else {
            cache.record_writeback_error_with_superblock(SystemError::EIO);
            return Err(SystemError::EIO);
        };
        let _sync_retention =
            match InodeRetentionGuard::new(sync_inode.clone(), InodeRetentionKind::AsyncWork) {
                Ok(guard) => guard,
                Err(error) => {
                    cache.record_writeback_error_with_superblock(error.clone());
                    return Err(error);
                }
            };
        self.sync_data_admitted(0, usize::MAX, file_size)?;

        let wbc = WritebackControl::sync_all_for_sync();
        if let Err(error) = sync_inode.write_inode(&wbc) {
            cache.record_writeback_error_with_superblock(error.clone());
            return Err(error);
        }
        Ok(())
    }

    /// Write and wait for every dirty or in-flight page in an inclusive range.
    ///
    /// Unlike `writeback_range`, this is a data-integrity operation: pages
    /// already under writeback when the call starts must complete before the
    /// caller may issue a backend fsync request.
    pub fn sync_range(&self, start_index: usize, end_index: usize) -> Result<(), SystemError> {
        self.sync_data(start_index, end_index)
    }

    /// Range counterpart of `sync_with_stable_size` for filesystem-private
    /// wrappers which already hold their write admission barrier.
    pub(crate) fn sync_range_with_stable_size(
        &self,
        start_index: usize,
        end_index: usize,
        file_size: usize,
    ) -> Result<(), SystemError> {
        self.sync_data_admitted(start_index, end_index, file_size)
    }

    pub fn writeback_range(&self, start_index: usize, end_index: usize) -> Result<(), SystemError> {
        self.sync_data(start_index, end_index)
    }

    /// Try at most one batch and return without waiting for writeback progress.
    ///
    /// Delayed-allocation progress workers use this entry point because the
    /// ordinary `writeback_range()` is a synchronous consumer: waiting there
    /// after the backend returns `Deferred` would make the producer wait on
    /// its own progress ticket.
    pub(crate) fn dispatch_writeback_once(
        &self,
        start_index: usize,
        end_index: usize,
        admitted: &super::PageCacheDomainIoPermit,
    ) -> Result<PageCacheWritebackDispatchOutcome, SystemError> {
        let cache = self.upgrade()?;
        let inode = cache
            .inode()
            .and_then(|inode| inode.upgrade())
            .ok_or(SystemError::EIO)?;
        match self.writeback_next_batch(&cache, &inode, start_index, end_index, Some(admitted))? {
            WritebackNextBatchOutcome::NoBatch => Ok(PageCacheWritebackDispatchOutcome::Idle),
            WritebackNextBatchOutcome::Completed => Ok(PageCacheWritebackDispatchOutcome::Progress),
            WritebackNextBatchOutcome::Deferred(_) => {
                Ok(PageCacheWritebackDispatchOutcome::Deferred)
            }
        }
    }

    pub fn wait_writeback_range(
        &self,
        start_index: usize,
        end_index: usize,
    ) -> Result<(), SystemError> {
        let cache = self.upgrade()?;
        Self::wait_writeback_range_bounded(&cache, start_index, end_index)
    }

    /// Launder a complete range for best-effort cache invalidation.
    ///
    /// Ordinary range writeback is fail-fast. Invalidation instead mirrors
    /// Linux invalidate_inode_pages2_range(): retain the first error but keep
    /// processing every later page in the range.
    pub(crate) fn launder_range_for_invalidate_with_stable_size(
        &self,
        start_index: usize,
        end_index: usize,
        file_size: usize,
    ) -> Result<(), SystemError> {
        if start_index > end_index {
            return Ok(());
        }
        let cache = self.upgrade()?;
        let mut first_error = None;
        loop {
            let mut cursor = start_index;
            while cursor <= end_index {
                let prepared = {
                    let _invalidate = cache.invalidate_read();
                    match Self::claim_next_writeback_batch(
                        &cache, cursor, end_index, file_size, None, false, None,
                    ) {
                        Ok(WritebackClaimOutcome::Claimed(mut batch)) => {
                            pc_stats::record_invalidation_launder_batch(batch.entries.len());
                            let last_index = batch
                                .entries
                                .last()
                                .map(|(index, _, _)| *index)
                                .unwrap_or(cursor);
                            let snapshot_result = Self::snapshot_writeback_batch(&mut batch);
                            Ok(Some((batch, last_index, snapshot_result)))
                        }
                        Ok(WritebackClaimOutcome::NoBatch) => Ok(None),
                        Ok(WritebackClaimOutcome::Deferred(_)) => {
                            unreachable!("invalidation laundering does not bind writeback tokens")
                        }
                        Ok(WritebackClaimOutcome::FailedRecorded(error)) => Err(error),
                        Err(error) => Err(error),
                    }
                };
                let Some((batch, last_index, snapshot_result)) = (match prepared {
                    Ok(prepared) => prepared,
                    Err(error) => {
                        first_error.get_or_insert(error);
                        break;
                    }
                }) else {
                    break;
                };
                let result = match snapshot_result {
                    Ok(()) => Self::submit_writeback_batch(batch),
                    Err(error) => Self::complete_writeback_batch(batch, Err(error))
                        .map(|_| WritebackSubmitOutcome::Completed),
                };
                if let Err(error) = result {
                    first_error.get_or_insert(error);
                }
                if last_index == usize::MAX {
                    break;
                }
                cursor = last_index + 1;
            }

            if let Err(error) = Self::wait_writeback_range_bounded(&cache, start_index, end_index) {
                first_error.get_or_insert(error);
            }
            if let Some(error) = first_error.take() {
                return Err(error);
            }

            let inner = cache.inner.lock();
            let has_dirty = inner
                .dirty_pages
                .range(start_index..=end_index)
                .next()
                .is_some();
            let has_writeback = inner
                .writeback_pages
                .range(start_index..=end_index)
                .next()
                .is_some();
            if !has_dirty && !has_writeback {
                return Ok(());
            }
            // A generation may have been redirtied behind an older Writeback
            // before the filesystem barrier became exclusive. The old
            // completion restores it to Dirty; restart from the range head so
            // invalidation cannot report success with that generation left.
        }
    }
}

impl PageCacheManager {
    pub(super) fn notify_tagged_writeback_progress(cache: &PageCache) {
        cache
            .tagged_writeback_progress
            .fetch_add(1, Ordering::AcqRel);
        cache.tagged_writeback_wait.wake_all();
    }

    /// Register the replacement predicate for a tag being cleared by a
    /// tagged claim.  The caller must hold `tagged_writeback_lock` from before
    /// the page-cache transition through this insertion.
    fn begin_tagged_writeback_submission_locked(
        cache: &PageCache,
        epoch: Option<u64>,
        first_index: usize,
        last_index: usize,
    ) {
        let Some(epoch) = epoch else {
            return;
        };
        cache
            .tagged_writeback_submissions
            .lock()
            .push(TaggedWritebackSubmission {
                epoch,
                first_index,
                last_index,
            });
    }

    fn finish_tagged_writeback_submission(
        cache: &PageCache,
        epoch: Option<u64>,
        first_index: usize,
        last_index: usize,
    ) {
        let Some(epoch) = epoch else {
            return;
        };
        let _tagged_writeback_transition = cache.tagged_writeback_lock.lock();
        let mut pending = cache.tagged_writeback_submissions.lock();
        let Some(position) = pending.iter().position(|submission| {
            submission.epoch == epoch
                && submission.first_index == first_index
                && submission.last_index == last_index
        }) else {
            // Every tagged batch registers after snapshot and finishes once
            // its PageCache completion and errseq publication are visible. A missing record is an internal
            // state-machine violation, not a condition to mask with retry.
            panic!("tagged writeback submission completion lost its registration");
        };
        pending.swap_remove(position);
        drop(pending);
        drop(_tagged_writeback_transition);
        Self::notify_tagged_writeback_progress(cache);
    }

    fn has_pending_tagged_writeback_submission(
        cache: &PageCache,
        start_index: usize,
        end_index: usize,
        epoch: u64,
    ) -> bool {
        cache
            .tagged_writeback_submissions
            .lock()
            .iter()
            .any(|submission| {
                submission.epoch != 0
                    && submission.epoch <= epoch
                    && submission.first_index <= end_index
                    && start_index <= submission.last_index
            })
    }

    /// Return whether this exact frozen generation still owns a Dirty tag.
    ///
    /// `WAIT_AFTER` intentionally treats an earlier epoch as pending for a
    /// later waiter, but a retry ticket must never inherit that earlier
    /// generation's work.  Ticket admission and truncate cancellation use
    /// this exact-owner predicate to avoid retaining a retry after its own
    /// final page disappears.
    pub(super) fn has_exact_tagged_writeback(
        cache: &PageCache,
        start_index: usize,
        end_index: usize,
        epoch: u64,
    ) -> bool {
        if start_index > end_index || epoch == 0 {
            return false;
        }
        let inner = cache.inner.lock();
        inner
            .dirty_pages
            .range(start_index..=end_index)
            .any(|index| {
                inner
                    .pages
                    .get(index)
                    .is_some_and(|entry| entry.writeback_tag() == epoch)
            })
    }

    /// Exact-owner variant of the submission predicate for retry tickets.
    /// See [`Self::has_exact_tagged_writeback`].
    pub(super) fn has_exact_tagged_writeback_submission(
        cache: &PageCache,
        start_index: usize,
        end_index: usize,
        epoch: u64,
    ) -> bool {
        epoch != 0
            && cache
                .tagged_writeback_submissions
                .lock()
                .iter()
                .any(|submission| {
                    submission.epoch == epoch
                        && submission.first_index <= end_index
                        && start_index <= submission.last_index
                })
    }

    fn abandon_tagged_writeback_generation(
        cache: &Arc<PageCache>,
        start_index: usize,
        frozen_end: usize,
        epoch: u64,
        error: SystemError,
    ) {
        // A bind failure happens before Dirty -> Writeback publication, so
        // there is no page-state transition to complete.  It is nevertheless
        // a writeback-start failure visible to an asynchronous caller; record
        // it in the mapping errseq and retire this generation's tags so a
        // WAIT_AFTER handle cannot sleep forever on untouched Dirty pages.
        Self::cancel_tagged_writeback_budget_retry(cache, epoch);
        cache.record_writeback_error_with_superblock(error);
        let _tagged_writeback_transition = cache.tagged_writeback_lock.lock();
        let inner = cache.inner.lock();
        for index in inner.dirty_pages.range(start_index..=frozen_end) {
            if let Some(entry) = inner.pages.get(index) {
                if entry.writeback_tag() == epoch {
                    entry.set_writeback_tag(0);
                }
            }
        }
        drop(inner);
        drop(_tagged_writeback_transition);
        Self::notify_tagged_writeback_progress(cache);
    }

    /// Retire a frozen generation without creating another errseq event.
    /// Callers use this after a terminal cancellation (where bytes became
    /// ineligible) or after a submission error which was already recorded by
    /// normal batch completion.
    pub(super) fn retire_tagged_writeback_generation(
        cache: &Arc<PageCache>,
        start_index: usize,
        frozen_end: usize,
        epoch: u64,
    ) {
        Self::cancel_tagged_writeback_budget_retry(cache, epoch);
        let _tagged_writeback_transition = cache.tagged_writeback_lock.lock();
        let inner = cache.inner.lock();
        for index in inner.dirty_pages.range(start_index..=frozen_end) {
            if let Some(entry) = inner.pages.get(index) {
                if entry.writeback_tag() == epoch {
                    entry.set_writeback_tag(0);
                }
            }
        }
        drop(inner);
        drop(_tagged_writeback_transition);
        Self::notify_tagged_writeback_progress(cache);
    }

    fn has_pending_tagged_writeback(
        cache: &PageCache,
        start_index: usize,
        end_index: usize,
        epoch: u64,
    ) -> bool {
        if start_index > end_index {
            return false;
        }
        let inner = cache.inner.lock();
        inner
            .dirty_pages
            .range(start_index..=end_index)
            .any(|index| {
                inner.pages.get(index).is_some_and(|entry| {
                    let tag = entry.writeback_tag();
                    // Tags are never overwritten while a request owns them.
                    // A smaller non-zero epoch therefore belongs to an
                    // earlier request whose deferred work this caller must
                    // not overtake; a later epoch is concurrent redirty and
                    // intentionally outside this request's frozen set.
                    tag != 0 && tag <= epoch
                })
            })
    }

    fn wait_tagged_writeback_range(
        cache: &Arc<PageCache>,
        start_index: usize,
        frozen_end: Option<usize>,
        writeback_frontier: u64,
        epoch: u64,
    ) -> Result<(), SystemError> {
        let Some(end_index) = frozen_end else {
            return Ok(());
        };
        loop {
            Self::wait_tagged_writeback_submission(cache, start_index, end_index, epoch)?;

            // Claim clears the tag immediately before publishing Writeback.
            // A submission-time defer can restore it after this wait, so the
            // final predicate must be checked again rather than trusting an
            // empty Writeback set as operation completion.
            Self::wait_writeback_range_bounded_through(
                cache,
                start_index,
                end_index,
                Some(writeback_frontier),
            )?;
            let _tagged_writeback_transition = cache.tagged_writeback_lock.lock();
            if !Self::has_pending_tagged_writeback(cache, start_index, end_index, epoch)
                && !Self::has_pending_tagged_writeback_submission(
                    cache,
                    start_index,
                    end_index,
                    epoch,
                )
            {
                return Ok(());
            }
        }
    }

    pub(super) fn wait_tagged_writeback_submission(
        cache: &Arc<PageCache>,
        start_index: usize,
        end_index: usize,
        epoch: u64,
    ) -> Result<(), SystemError> {
        loop {
            let observed = {
                let _tagged_writeback_transition = cache.tagged_writeback_lock.lock();
                let pending =
                    Self::has_pending_tagged_writeback(cache, start_index, end_index, epoch)
                        || Self::has_pending_tagged_writeback_submission(
                            cache,
                            start_index,
                            end_index,
                            epoch,
                        );
                if !pending {
                    return Ok(());
                }
                cache.tagged_writeback_progress.load(Ordering::Acquire)
            };
            // Recheck the exact predicate after sampling its sequence.
            // A deferred ticket's continuation advances this sequence
            // only after its producer has progressed and before the
            // retry revalidates PageCache state, closing both lost-wake
            // and Dirty-page polling holes.
            cache.tagged_writeback_wait.wait_until(|| {
                let _tagged_writeback_transition = cache.tagged_writeback_lock.lock();
                if (!Self::has_pending_tagged_writeback(cache, start_index, end_index, epoch)
                    && !Self::has_pending_tagged_writeback_submission(
                        cache,
                        start_index,
                        end_index,
                        epoch,
                    ))
                    || cache.tagged_writeback_progress.load(Ordering::Acquire) != observed
                {
                    Some(())
                } else {
                    None
                }
            });
        }
    }

    fn find_tagged_writeback_target(
        cache: &Arc<PageCache>,
        cursor: usize,
        frozen_end: usize,
        epoch: u64,
    ) -> TaggedWritebackSearch {
        const WRITEBACK_TAG_CHUNK: usize = 256;
        if cursor > frozen_end {
            return TaggedWritebackSearch::Done;
        }
        let inner = cache.inner.lock();
        let mut last_scanned = None;
        for index in inner
            .dirty_pages
            .range(cursor..=frozen_end)
            .take(WRITEBACK_TAG_CHUNK)
        {
            last_scanned = Some(*index);
            let Some(entry) = inner.pages.get(index) else {
                continue;
            };
            if entry.writeback_tag() != epoch {
                continue;
            }
            let mut tagged_end = *index;
            for next in inner
                .dirty_pages
                .range(*index..=frozen_end)
                .skip(1)
                .take(WRITEBACK_TAG_CHUNK - 1)
            {
                if *next != tagged_end.saturating_add(1) {
                    break;
                }
                let Some(next_entry) = inner.pages.get(next) else {
                    break;
                };
                if next_entry.writeback_tag() != epoch {
                    break;
                }
                tagged_end = *next;
            }
            return TaggedWritebackSearch::Target {
                index: *index,
                entry: entry.clone(),
                end: tagged_end,
            };
        }
        match last_scanned {
            Some(index) if index < frozen_end && index != usize::MAX => {
                TaggedWritebackSearch::Advance(index + 1)
            }
            _ => TaggedWritebackSearch::Done,
        }
    }

    /// Register an exact completion continuation for a tagged page redirtied
    /// behind an older Writeback incarnation.
    ///
    /// Registration precedes the completion recheck. The completion path and
    /// this recheck both remove under the same retry lock, so a racing old I/O
    /// either observes the continuation or has already made it dispatchable;
    /// neither side can lose or dispatch it twice.
    fn register_tagged_writeback_incarnation_retry(
        cache: &Arc<PageCache>,
        inode: Weak<dyn IndexNode>,
        entry: Arc<PageEntry>,
        observed_incarnation: u64,
        continuation: TaggedWritebackCursor,
    ) -> Result<(), SystemError> {
        {
            let mut pending = cache.writeback_incarnation_retries.lock();
            pending.try_reserve(1).map_err(|_| SystemError::ENOMEM)?;
            pending.push(TaggedWritebackIncarnationRetry {
                entry: entry.clone(),
                observed_incarnation,
                inode,
                continuation,
            });
        }
        Self::dispatch_writeback_incarnation_retries(cache, &entry, observed_incarnation);
        Ok(())
    }

    fn dispatch_writeback_incarnation_retries(
        cache: &Arc<PageCache>,
        completed_entry: &Arc<PageEntry>,
        completed_incarnation: u64,
    ) {
        loop {
            let retry = {
                let mut pending = cache.writeback_incarnation_retries.lock();
                pending
                    .iter()
                    .position(|retry| {
                        Arc::ptr_eq(&retry.entry, completed_entry)
                            && retry.observed_incarnation == completed_incarnation
                            && Self::writeback_incarnation_result(
                                &retry.entry,
                                retry.observed_incarnation,
                            )
                            .is_some()
                    })
                    .map(|index| pending.swap_remove(index))
            };
            let Some(retry) = retry else {
                break;
            };
            let TaggedWritebackCursor {
                start_index,
                frozen_end,
                epoch,
                cursor,
            } = retry.continuation;
            Self::schedule_tagged_writeback_drain_with_permit(
                Arc::downgrade(cache),
                retry.inode,
                start_index,
                frozen_end,
                epoch,
                cursor,
                None,
            );
        }
    }

    fn schedule_tagged_writeback_drain_with_permit(
        cache: Weak<PageCache>,
        inode: Weak<dyn IndexNode>,
        start_index: usize,
        frozen_end: usize,
        epoch: u64,
        cursor: usize,
        permit: Option<AsyncWritebackPermit>,
    ) {
        let Some(cache_arc) = cache.upgrade() else {
            return;
        };
        let domain_io = match cache_arc.try_acquire_domain_io_classified() {
            Ok(permit) => permit,
            Err(super::PageCacheDomainIoAdmissionError::Closed) => {
                Self::cancel_tagged_writeback_budget_retry(&cache_arc, epoch);
                Self::retire_tagged_writeback_generation(
                    &cache_arc,
                    start_index,
                    frozen_end,
                    epoch,
                );
                return;
            }
            Err(super::PageCacheDomainIoAdmissionError::Unavailable(error)) => {
                Self::cancel_tagged_writeback_budget_retry(&cache_arc, epoch);
                Self::abandon_tagged_writeback_generation(
                    &cache_arc,
                    start_index,
                    frozen_end,
                    epoch,
                    error,
                );
                return;
            }
        };
        drop(cache_arc);
        let work_state = Mutex::new(Some((permit, domain_io)));
        schedule_pagecache_writeback(Work::new(move || {
            let Some((permit, domain_io)) = work_state.lock().take() else {
                return;
            };
            let Some(cache) = cache.upgrade() else {
                return;
            };
            let Some(inode) = inode.upgrade() else {
                Self::cancel_tagged_writeback_budget_retry(&cache, epoch);
                Self::abandon_tagged_writeback_generation(
                    &cache,
                    start_index,
                    frozen_end,
                    epoch,
                    SystemError::EIO,
                );
                return;
            };
            let manager = cache.manager.clone();
            manager.drain_tagged_writeback(
                &cache,
                &inode,
                TaggedWritebackCursor {
                    start_index,
                    frozen_end,
                    epoch,
                    cursor,
                },
                permit,
                domain_io,
            );
        }));
    }

    /// Queue one exact-cursor retry for this frozen generation.  The cache
    /// coalesces repeated saturation observations into one ticket and a
    /// release transfers its permit to that ticket in FIFO order.
    fn schedule_tagged_writeback_budget_retry(
        cache: &Arc<PageCache>,
        inode: &Arc<dyn IndexNode>,
        start_index: usize,
        frozen_end: usize,
        epoch: u64,
        cursor: usize,
    ) {
        let Some(ticket) = Self::arm_tagged_writeback_budget_retry(
            cache,
            Some(Arc::downgrade(inode)),
            start_index,
            frozen_end,
            epoch,
            cursor,
        ) else {
            return;
        };
        AsyncWritebackPermit::register_retry(ticket);
    }

    /// Create the one retry ticket owned by a frozen generation, or merge a
    /// duplicate saturation observation into its earliest safe cursor.
    fn arm_tagged_writeback_budget_retry(
        cache: &Arc<PageCache>,
        inode: Option<Weak<dyn IndexNode>>,
        start_index: usize,
        frozen_end: usize,
        epoch: u64,
        cursor: usize,
    ) -> Option<Arc<AsyncWritebackRetryTicket>> {
        // Serialize the predicate check and ticket insertion with truncate's
        // final tagged-page removal.  Without this lock a drain could see a
        // tag, then truncate could remove it, and finally the drain could
        // append an orphan retry after truncate had already swept the queue.
        let _tagged_writeback_transition = cache.tagged_writeback_lock.lock();
        if !Self::has_exact_tagged_writeback(cache, start_index, frozen_end, epoch)
            && !Self::has_exact_tagged_writeback_submission(cache, start_index, frozen_end, epoch)
        {
            return None;
        }
        let ticket = {
            let mut retries = cache.tagged_writeback_budget_retries.lock();
            if let Some(existing) = retries.get_mut(&epoch) {
                // The generation has one immutable frozen range.  A racing
                // continuation may have advanced further, but retrying an
                // earlier cursor is always safe and cannot skip a tag.
                existing.cursor = existing.cursor.min(cursor);
                return None;
            }
            let ticket = Arc::new(AsyncWritebackRetryTicket::new(Arc::downgrade(cache), epoch));
            retries.insert(
                epoch,
                TaggedWritebackBudgetRetry {
                    ticket: ticket.clone(),
                    inode,
                    start_index,
                    frozen_end,
                    cursor,
                    state: TaggedWritebackBudgetRetryState::Queued,
                },
            );
            ticket
        };
        Some(ticket)
    }

    pub(super) fn cancel_tagged_writeback_budget_retry(cache: &PageCache, epoch: u64) {
        let ticket = cache
            .tagged_writeback_budget_retries
            .lock()
            .remove(&epoch)
            .map(|retry| retry.ticket);
        let Some(ticket) = ticket else {
            return;
        };
        ticket.cancelled.store(true, Ordering::Release);
        ASYNC_WRITEBACK_RETRIES
            .lock()
            .retain(|queued| !Arc::ptr_eq(&ticket, queued));
    }

    fn resume_tagged_writeback_budget_retry(
        &self,
        cache: &Arc<PageCache>,
        ticket: &Arc<AsyncWritebackRetryTicket>,
        permit: AsyncWritebackPermit,
    ) {
        let retry = {
            let mut retries = cache.tagged_writeback_budget_retries.lock();
            let Some(retry) = retries.get_mut(&ticket.epoch) else {
                return;
            };
            if !Arc::ptr_eq(&retry.ticket, ticket)
                || retry.state != TaggedWritebackBudgetRetryState::Queued
            {
                return;
            }
            retry.state = TaggedWritebackBudgetRetryState::Granted;
            (
                retry.inode.clone(),
                retry.start_index,
                retry.frozen_end,
                retry.cursor,
            )
        };
        let Some(inode) = retry.0 else {
            Self::cancel_tagged_writeback_budget_retry(cache, ticket.epoch);
            return;
        };
        Self::schedule_tagged_writeback_drain_with_permit(
            Arc::downgrade(cache),
            inode,
            retry.1,
            retry.2,
            ticket.epoch,
            retry.3,
            Some(permit),
        );
    }

    fn schedule_tagged_writeback_submission(
        cache: Weak<PageCache>,
        inode: Weak<dyn IndexNode>,
        continuation: TaggedWritebackCursor,
        last_index: usize,
        permit: AsyncWritebackPermit,
        batch: ClaimedWritebackBatch,
    ) {
        let TaggedWritebackCursor {
            start_index,
            frozen_end,
            epoch,
            cursor: retry_cursor,
        } = continuation;
        let work_state = Mutex::new(Some((permit, batch)));
        schedule_pagecache_writeback(Work::new(move || {
            let Some((permit, batch)) = work_state.lock().take() else {
                return;
            };
            let outcome = Self::submit_writeback_batch(batch);
            drop(permit);
            match outcome {
                Ok(WritebackSubmitOutcome::Completed) => {
                    if last_index == usize::MAX {
                        if let Some(cache) = cache.upgrade() {
                            Self::notify_tagged_writeback_progress(&cache);
                        }
                        return;
                    }
                    Self::schedule_tagged_writeback_drain_with_permit(
                        cache.clone(),
                        inode.clone(),
                        start_index,
                        frozen_end,
                        epoch,
                        last_index + 1,
                        None,
                    );
                }
                Ok(WritebackSubmitOutcome::Deferred(progress)) => {
                    Self::schedule_tagged_writeback_retry(
                        progress,
                        cache.clone(),
                        inode.clone(),
                        start_index,
                        frozen_end,
                        epoch,
                        retry_cursor,
                    );
                }
                Ok(WritebackSubmitOutcome::Failed(_)) => {
                    if let Some(cache) = cache.upgrade() {
                        // Batch completion already recorded the error. Do not
                        // leave later tags waiting for a continuation that
                        // cannot be scheduled after a terminal failure.
                        Self::retire_tagged_writeback_generation(
                            &cache,
                            start_index,
                            frozen_end,
                            epoch,
                        );
                    }
                }
                Err(error) => {
                    if let Some(cache) = cache.upgrade() {
                        Self::abandon_tagged_writeback_generation(
                            &cache,
                            start_index,
                            frozen_end,
                            epoch,
                            error,
                        );
                    }
                }
            }
        }));
    }

    fn schedule_tagged_writeback_retry(
        progress: Arc<dyn PageCacheWritebackProgress>,
        cache: Weak<PageCache>,
        inode: Weak<dyn IndexNode>,
        start_index: usize,
        frozen_end: usize,
        epoch: u64,
        cursor: usize,
    ) {
        progress.arm();
        let invoked = Arc::new(AtomicBool::new(false));
        progress.register_retry(Arc::new(move |outcome| {
            // The trait requires exactly once, but retain a PageCache-side
            // one-shot guard as a containment boundary for a buggy future
            // backend: duplicate callbacks must not create two head drains.
            if invoked.swap(true, Ordering::AcqRel) {
                return;
            }
            // The backend invokes this only after producer progress or a
            // terminal cancellation/poison transition.  No PageCache worker
            // is parked on the ticket; the retry below revalidates all state.
            let Some(cache_arc) = cache.upgrade() else {
                return;
            };
            match outcome {
                PageCacheWritebackProgressOutcome::Progress => {
                    Self::notify_tagged_writeback_progress(&cache_arc);
                    drop(cache_arc);
                    Self::schedule_tagged_writeback_drain_with_permit(
                        cache.clone(),
                        inode.clone(),
                        start_index,
                        frozen_end,
                        epoch,
                        cursor,
                        None,
                    );
                }
                PageCacheWritebackProgressOutcome::Cancelled => {
                    // A ticket owns only its queue head, not every later
                    // page in this frozen generation. Truncate/unlink may
                    // have removed that head while successors remain valid;
                    // revalidate from the same cursor instead of clearing
                    // the whole epoch and letting WAIT_AFTER finish early.
                    Self::notify_tagged_writeback_progress(&cache_arc);
                    drop(cache_arc);
                    Self::schedule_tagged_writeback_drain_with_permit(
                        cache.clone(),
                        inode.clone(),
                        start_index,
                        frozen_end,
                        epoch,
                        cursor,
                        None,
                    );
                }
                PageCacheWritebackProgressOutcome::Failed(error) => {
                    match cache_arc.try_acquire_domain_io_classified() {
                        Ok(_permit) => Self::abandon_tagged_writeback_generation(
                            &cache_arc,
                            start_index,
                            frozen_end,
                            epoch,
                            error,
                        ),
                        Err(super::PageCacheDomainIoAdmissionError::Closed) => {
                            Self::retire_tagged_writeback_generation(
                                &cache_arc,
                                start_index,
                                frozen_end,
                                epoch,
                            );
                        }
                        Err(super::PageCacheDomainIoAdmissionError::Unavailable(error)) => {
                            Self::abandon_tagged_writeback_generation(
                                &cache_arc,
                                start_index,
                                frozen_end,
                                epoch,
                                error,
                            );
                        }
                    }
                }
            }
        }));
    }

    fn schedule_reclaimer_deferred_retry(
        progress: Arc<dyn PageCacheWritebackProgress>,
        manager: PageCacheManager,
        start_index: usize,
        end_index: usize,
    ) {
        progress.arm();
        let invoked = Arc::new(AtomicBool::new(false));
        progress.register_retry(Arc::new(move |outcome| {
            if invoked.swap(true, Ordering::AcqRel) {
                return;
            }
            // Reclaim itself remains non-blocking.  The producer wake
            // re-enters the try-only path, which revalidates cache/inode/head
            // from scratch instead of trusting a stale ticket.
            match outcome {
                PageCacheWritebackProgressOutcome::Progress => {
                    let _ = manager.try_start_reclaimer_writeback_range(start_index, end_index);
                }
                PageCacheWritebackProgressOutcome::Cancelled => {}
                PageCacheWritebackProgressOutcome::Failed(error) => {
                    if let Ok(cache) = manager.upgrade() {
                        match cache.try_acquire_domain_io_classified() {
                            Ok(_permit) => cache.record_writeback_error_with_superblock(error),
                            Err(super::PageCacheDomainIoAdmissionError::Closed) => {}
                            Err(super::PageCacheDomainIoAdmissionError::Unavailable(error)) => {
                                cache.record_writeback_error(error);
                            }
                        }
                    }
                }
            }
        }));
    }

    fn drain_tagged_writeback(
        &self,
        cache: &Arc<PageCache>,
        inode: &Arc<dyn IndexNode>,
        continuation: TaggedWritebackCursor,
        mut reserved_permit: Option<AsyncWritebackPermit>,
        domain_io: Option<super::PageCacheDomainIoPermit>,
    ) {
        let TaggedWritebackCursor {
            start_index,
            frozen_end,
            epoch,
            mut cursor,
        } = continuation;
        // A granted ticket owns its permit until this worker begins.  Remove
        // the cache-side coalescing state before revalidation so every later
        // saturation can enqueue exactly one fresh ticket for the remaining
        // cursor, while a stale queued ticket cannot retain the cache.
        if reserved_permit.is_some() {
            Self::cancel_tagged_writeback_budget_retry(cache, epoch);
        }
        loop {
            let (target_index, target_entry, tagged_end) =
                match Self::find_tagged_writeback_target(cache, cursor, frozen_end, epoch) {
                    TaggedWritebackSearch::Done => {
                        Self::notify_tagged_writeback_progress(cache);
                        return;
                    }
                    TaggedWritebackSearch::Advance(next_cursor) => {
                        cursor = next_cursor;
                        crate::sched::sched_yield();
                        continue;
                    }
                    TaggedWritebackSearch::Target { index, entry, end } => (index, entry, end),
                };

            let permit = match reserved_permit.take() {
                Some(permit) => permit,
                None => match AsyncWritebackPermit::try_acquire() {
                    Some(permit) => permit,
                    None => {
                        // This drain is already executing on the bounded PageCache
                        // writeback pool. Never park that worker waiting for a slot:
                        // a FIFO permit handoff queues an exact-cursor retry instead.
                        Self::schedule_tagged_writeback_budget_retry(
                            cache,
                            inode,
                            start_index,
                            frozen_end,
                            epoch,
                            cursor,
                        );
                        return;
                    }
                },
            };
            let claim = match self.claim_tagged_batch_with_admission(
                cache,
                inode,
                WritebackBatchRange::new(target_index, tagged_end),
                &target_entry,
                epoch,
                domain_io.as_ref(),
            ) {
                Ok(claim) => claim,
                Err(error) => {
                    // Binding errors leave pages Dirty, but an asynchronous
                    // range has no caller stack on which to return them.
                    // Publish the mapping error and retire only this frozen
                    // generation; later dirty writeback may retry normally.
                    drop(permit);
                    Self::abandon_tagged_writeback_generation(
                        cache,
                        start_index,
                        frozen_end,
                        epoch,
                        error,
                    );
                    return;
                }
            };
            match claim {
                WritebackClaimOutcome::Deferred(progress) => {
                    drop(permit);
                    // No successor is examined until the exact queue-head
                    // ticket has progressed and this cursor is revalidated.
                    Self::schedule_tagged_writeback_retry(
                        progress,
                        Arc::downgrade(cache),
                        Arc::downgrade(inode),
                        start_index,
                        frozen_end,
                        epoch,
                        target_index,
                    );
                    return;
                }
                WritebackClaimOutcome::FailedRecorded(_) => {
                    drop(permit);
                    // PageCache completion already published the terminal
                    // error. Retire this frozen generation without creating
                    // a second errseq event.
                    Self::retire_tagged_writeback_generation(cache, start_index, frozen_end, epoch);
                    return;
                }
                WritebackClaimOutcome::NoBatch => {
                    drop(permit);
                    let retry_incarnation = {
                        let inner = cache.inner.lock();
                        inner.pages.get(&target_index).and_then(|current| {
                            (Arc::ptr_eq(current, &target_entry)
                                && inner.dirty_pages.contains(&target_index)
                                && current.writeback_tag() == epoch)
                                .then(|| current.writeback_incarnation.load(Ordering::Acquire))
                        })
                    };
                    if let Some(observed_incarnation) = retry_incarnation {
                        if let Err(error) = Self::register_tagged_writeback_incarnation_retry(
                            cache,
                            Arc::downgrade(inode),
                            target_entry,
                            observed_incarnation,
                            TaggedWritebackCursor {
                                start_index,
                                frozen_end,
                                epoch,
                                cursor: target_index,
                            },
                        ) {
                            Self::abandon_tagged_writeback_generation(
                                cache,
                                start_index,
                                frozen_end,
                                epoch,
                                error,
                            );
                        }
                        return;
                    }
                    if target_index == usize::MAX {
                        Self::notify_tagged_writeback_progress(cache);
                        return;
                    }
                    cursor = target_index + 1;
                }
                WritebackClaimOutcome::Claimed(batch) => {
                    let last_index = batch
                        .entries
                        .last()
                        .map(|(index, _, _)| *index)
                        .unwrap_or(target_index);
                    if batch.submission.is_none() {
                        // A Legacy backend has opted out of the token/defer
                        // protocol, so its write_pages() path cannot return
                        // Deferred.  Preserve the established parallel
                        // writeback throughput for those filesystems; only
                        // token-backed delayed allocation is serialized by
                        // submit outcome.
                        let work_state = Mutex::new(Some((permit, batch)));
                        schedule_pagecache_writeback(Work::new(move || {
                            let Some((permit, batch)) = work_state.lock().take() else {
                                return;
                            };
                            let _permit = permit;
                            let _ = Self::submit_writeback_batch(batch);
                        }));
                        if last_index == usize::MAX {
                            Self::notify_tagged_writeback_progress(cache);
                            return;
                        }
                        cursor = last_index + 1;
                        continue;
                    }
                    match Self::submit_writeback_batch(batch) {
                        Ok(WritebackSubmitOutcome::Completed) => {
                            drop(permit);
                            if last_index == usize::MAX {
                                Self::notify_tagged_writeback_progress(cache);
                                return;
                            }
                            // The submit result is now known.  Only a
                            // completed head permits claiming its successor.
                            cursor = last_index + 1;
                        }
                        Ok(WritebackSubmitOutcome::Deferred(progress)) => {
                            drop(permit);
                            Self::schedule_tagged_writeback_retry(
                                progress,
                                Arc::downgrade(cache),
                                Arc::downgrade(inode),
                                start_index,
                                frozen_end,
                                epoch,
                                target_index,
                            );
                            return;
                        }
                        Ok(WritebackSubmitOutcome::Failed(_)) => {
                            drop(permit);
                            // The failed batch has already recorded its
                            // errseq in normal completion. Leaving later
                            // tags behind would strand WAIT_AFTER forever;
                            // retire them without recording that same error
                            // a second time.
                            Self::retire_tagged_writeback_generation(
                                cache,
                                start_index,
                                frozen_end,
                                epoch,
                            );
                            return;
                        }
                        Err(_) => {
                            drop(permit);
                            Self::notify_tagged_writeback_progress(cache);
                            return;
                        }
                    }
                }
            }
        }
    }

    pub fn start_writeback_range(
        &self,
        start_index: usize,
        end_index: usize,
    ) -> Result<PageCacheWritebackRange, SystemError> {
        self.start_writeback_range_with_freeze(start_index, end_index, || Ok(()))
            .map(|(_, range)| range)
    }

    /// Freeze filesystem metadata and the matching page-cache dirty
    /// generation at one invalidate-write snapshot boundary.
    ///
    /// The callback must follow the filesystem's normal lock order below the
    /// PageCache invalidate lock. Tag publication then proceeds in bounded
    /// invalidate-write chunks. Dispatch begins only after the final chunk is
    /// released, because backend admission itself takes invalidate-read.
    pub(crate) fn start_writeback_range_with_freeze<T, F>(
        &self,
        start_index: usize,
        end_index: usize,
        freeze: F,
    ) -> Result<(T, PageCacheWritebackRange), SystemError>
    where
        F: FnOnce() -> Result<T, SystemError>,
    {
        self.start_writeback_range_with_freeze_and_chunk_release(
            start_index,
            end_index,
            freeze,
            || {
                crate::sched::sched_yield();
            },
        )
    }

    /// Internal form with an explicit action at each released chunk boundary.
    /// Production uses it to yield; the selftest uses it to verify that the
    /// invalidate writer is actually absent without relying on scheduler
    /// timing.
    pub(super) fn start_writeback_range_with_freeze_and_chunk_release<T, F, C>(
        &self,
        start_index: usize,
        end_index: usize,
        freeze: F,
        mut chunk_released: C,
    ) -> Result<(T, PageCacheWritebackRange), SystemError>
    where
        F: FnOnce() -> Result<T, SystemError>,
        C: FnMut(),
    {
        let cache = self.upgrade()?;
        // Keep one freeze scanner's epoch from being split by a later
        // scanner while `invalidate_write` is released between chunks.
        // Truncate and fault paths do not take this lock, so they can make
        // progress at every chunk boundary.
        let _tag_scan = cache.writeback_tag_scan_lock.lock();
        let mut invalidate = cache.invalidate_write();
        let frozen_filesystem_state = freeze()?;
        if start_index > end_index {
            return Ok((
                frozen_filesystem_state,
                PageCacheWritebackRange {
                    cache: Arc::downgrade(&cache),
                    start_index,
                    frozen_end: None,
                    writeback_frontier: 0,
                    epoch: 0,
                },
            ));
        }

        // Freeze the caller-visible dirty set with an epoch tag, equivalent to
        // Linux's PAGECACHE_TAG_TOWRITE pass but without materializing an
        // unbounded Vec. A monotonic index walk bounds transient memory and
        // prevents a low-index redirtier from starving later pages.
        const WRITEBACK_TAG_CHUNK: usize = 256;
        let (preexisting_writeback_end, dirty_end) = {
            let inner = cache.inner.lock();
            (
                inner
                    .writeback_pages
                    .range(start_index..=end_index)
                    .next_back()
                    .copied(),
                inner
                    .dirty_pages
                    .range(start_index..=end_index)
                    .next_back()
                    .copied(),
            )
        };
        let epoch = {
            let _tagged_writeback = cache.tagged_writeback_lock.lock();
            // The numeric epoch is also the request-generation order used by
            // WAIT_AFTER.  It must therefore be assigned under the same lock
            // which linearizes tag publication; allocating it before this
            // point can invert two concurrent callers and make an earlier
            // waiter ignore the generation that actually froze first.
            let mut epoch = PAGE_CACHE_WRITEBACK_TAG_EPOCH.fetch_add(1, Ordering::AcqRel);
            if epoch == 0 {
                epoch = PAGE_CACHE_WRITEBACK_TAG_EPOCH.fetch_add(1, Ordering::AcqRel);
            }
            epoch
        };
        let Some(frozen_end) = dirty_end.into_iter().chain(preexisting_writeback_end).max() else {
            return Ok((
                frozen_filesystem_state,
                PageCacheWritebackRange {
                    cache: Arc::downgrade(&cache),
                    start_index,
                    frozen_end: None,
                    writeback_frontier: 0,
                    epoch,
                },
            ));
        };
        let mut tagged_new = false;
        let mut tag_cursor = start_index;
        let writeback_frontier = loop {
            let last_scanned = {
                let _tagged_writeback = cache.tagged_writeback_lock.lock();
                {
                    let inner = cache.inner.lock();
                    let mut last = None;
                    for index in inner
                        .dirty_pages
                        .range(tag_cursor..=frozen_end)
                        .take(WRITEBACK_TAG_CHUNK)
                    {
                        if let Some(entry) = inner.pages.get(index) {
                            // Do not overwrite an earlier caller's frozen
                            // generation.  That caller may currently be
                            // waiting for a deferred queue head; both ranges
                            // can share the eventual writeback, but only the
                            // owner of this epoch may drain it.
                            if entry.writeback_tag() == 0 {
                                entry.set_writeback_tag(epoch);
                                tagged_new = true;
                            }
                            last = Some(*index);
                        }
                    }
                    last
                }
            };
            let scan_complete = last_scanned.is_none_or(|last| last >= frozen_end);
            if scan_complete {
                // An ordinary writeback claimant may have moved an initially
                // dirty page to Writeback while invalidate-write was released
                // between chunks. Sample the final incarnation frontier while
                // the last writer exclusion is still held, so every such I/O
                // is either complete already or bounded by this operation.
                let inner = cache.inner.lock();
                break inner.next_writeback_incarnation.saturating_sub(1);
            }

            tag_cursor = last_scanned
                .expect("an incomplete tag scan must have examined an index")
                .checked_add(1)
                .expect("an incomplete tag scan cannot end at usize::MAX");
            drop(invalidate);
            // Match Linux tag_pages_for_writeback(): release the mapping
            // exclusion as well as the page-cache index lock between bounded
            // chunks, then allow queued faults and invalidators to run.
            chunk_released();
            invalidate = cache.invalidate_write();
        };
        let operation = PageCacheWritebackRange {
            cache: Arc::downgrade(&cache),
            start_index,
            frozen_end: Some(frozen_end),
            writeback_frontier,
            epoch,
        };
        // Backend claim/admission takes invalidate-read. Release the writer
        // only after both metadata and page tags have been frozen.
        drop(invalidate);
        if !tagged_new {
            return Ok((frozen_filesystem_state, operation));
        }

        let inode = match cache.inode().and_then(|inode| inode.upgrade()) {
            Some(inode) => inode,
            None => {
                Self::abandon_tagged_writeback_generation(
                    &cache,
                    start_index,
                    frozen_end,
                    epoch,
                    SystemError::EIO,
                );
                return Err(SystemError::EIO);
            }
        };
        let _domain_io = match cache.try_acquire_domain_io_classified() {
            Ok(permit) => permit,
            Err(super::PageCacheDomainIoAdmissionError::Closed) => {
                Self::retire_tagged_writeback_generation(&cache, start_index, frozen_end, epoch);
                return Err(SystemError::ESTALE);
            }
            Err(super::PageCacheDomainIoAdmissionError::Unavailable(error)) => {
                Self::abandon_tagged_writeback_generation(
                    &cache,
                    start_index,
                    frozen_end,
                    epoch,
                    error.clone(),
                );
                return Err(error);
            }
        };

        // `SYNC_FILE_RANGE_WRITE` is an asynchronous writeout starter: it
        // publishes Dirty -> Writeback and queues legacy I/O, but must not
        // run a synchronous backend write_pages() to completion on the
        // syscall stack. Tagged batches additionally register a precise
        // submission-boundary record. WAIT_BEFORE|WRITE waits for workers to
        // cross that record, while ordinary WRITE returns after dispatch.
        //
        // A token queue stops after its first claimed head. Its submit result
        // or defer continuation is the only path allowed to inspect a
        // successor, preserving delayed-allocation head-first order.
        let mut cursor = start_index;
        loop {
            let (target_index, target_entry, tagged_end) =
                match Self::find_tagged_writeback_target(&cache, cursor, frozen_end, epoch) {
                    TaggedWritebackSearch::Done => {
                        Self::notify_tagged_writeback_progress(&cache);
                        break;
                    }
                    TaggedWritebackSearch::Advance(next_cursor) => {
                        cursor = next_cursor;
                        crate::sched::sched_yield();
                        continue;
                    }
                    TaggedWritebackSearch::Target { index, entry, end } => (index, entry, end),
                };

            let Some(permit) = AsyncWritebackPermit::try_acquire() else {
                // WRITE is an asynchronous starter. A saturated batch budget
                // leaves the next frozen tag Dirty and registers a one-shot
                // drain retry; it must not wait for an older write_pages()
                // call to finish on the syscall stack.
                Self::schedule_tagged_writeback_budget_retry(
                    &cache,
                    &inode,
                    start_index,
                    frozen_end,
                    epoch,
                    cursor,
                );
                break;
            };
            let claim = match self.claim_tagged_batch_with_admission(
                &cache,
                &inode,
                WritebackBatchRange::new(target_index, tagged_end),
                &target_entry,
                epoch,
                _domain_io.as_ref(),
            ) {
                Ok(claim) => claim,
                Err(error) => {
                    drop(permit);
                    Self::abandon_tagged_writeback_generation(
                        &cache,
                        start_index,
                        frozen_end,
                        epoch,
                        error.clone(),
                    );
                    return Err(error);
                }
            };
            match claim {
                WritebackClaimOutcome::Deferred(progress) => {
                    drop(permit);
                    // The callback is registered before WRITE returns, so a
                    // claim-time defer has a producer-owned progress edge
                    // rather than relying on an unrelated later reclaim.
                    Self::schedule_tagged_writeback_retry(
                        progress,
                        Arc::downgrade(&cache),
                        Arc::downgrade(&inode),
                        start_index,
                        frozen_end,
                        epoch,
                        target_index,
                    );
                    break;
                }
                WritebackClaimOutcome::FailedRecorded(error) => {
                    drop(permit);
                    // The claimed batch has already been completed back to
                    // Dirty and its error recorded by PageCache. Only retire
                    // the remaining frozen tags here.
                    Self::retire_tagged_writeback_generation(
                        &cache,
                        start_index,
                        frozen_end,
                        epoch,
                    );
                    return Err(error);
                }
                WritebackClaimOutcome::NoBatch => {
                    drop(permit);
                    let retry_incarnation = {
                        let inner = cache.inner.lock();
                        inner.pages.get(&target_index).and_then(|current| {
                            (Arc::ptr_eq(current, &target_entry)
                                && inner.dirty_pages.contains(&target_index)
                                && current.writeback_tag() == epoch)
                                .then(|| current.writeback_incarnation.load(Ordering::Acquire))
                        })
                    };
                    if let Some(observed_incarnation) = retry_incarnation {
                        if let Err(error) = Self::register_tagged_writeback_incarnation_retry(
                            &cache,
                            Arc::downgrade(&inode),
                            target_entry,
                            observed_incarnation,
                            TaggedWritebackCursor {
                                start_index,
                                frozen_end,
                                epoch,
                                cursor: target_index,
                            },
                        ) {
                            Self::abandon_tagged_writeback_generation(
                                &cache,
                                start_index,
                                frozen_end,
                                epoch,
                                error.clone(),
                            );
                            return Err(error);
                        }
                        break;
                    }
                    if target_index == usize::MAX {
                        Self::notify_tagged_writeback_progress(&cache);
                        break;
                    }
                    cursor = target_index + 1;
                }
                WritebackClaimOutcome::Claimed(batch) => {
                    let last_index = batch
                        .entries
                        .last()
                        .map(|(index, _, _)| *index)
                        .unwrap_or(target_index);
                    if batch.submission.is_none() {
                        // Legacy has no Deferred outcome, so it may retain
                        // established parallel background writeback. The
                        // worker clears the per-generation submission record
                        // only after PageCache completion and errseq
                        // publication make the batch result observable.
                        let work_state = Mutex::new(Some((permit, batch)));
                        schedule_pagecache_writeback(Work::new(move || {
                            let Some((permit, batch)) = work_state.lock().take() else {
                                return;
                            };
                            let _permit = permit;
                            let _ = Self::submit_writeback_batch(batch);
                        }));
                        if last_index == usize::MAX {
                            break;
                        }
                        cursor = last_index + 1;
                        continue;
                    }

                    Self::schedule_tagged_writeback_submission(
                        Arc::downgrade(&cache),
                        Arc::downgrade(&inode),
                        TaggedWritebackCursor {
                            start_index,
                            frozen_end,
                            epoch,
                            cursor: target_index,
                        },
                        last_index,
                        permit,
                        batch,
                    );
                    break;
                }
            }
        }

        Ok((frozen_filesystem_state, operation))
    }

    /// Schedule one bounded reclaimer batch without doing page/MM work on the
    /// reclaim thread. A busy runner or global budget leaves pages Dirty for a
    /// later round; lock-taking claim/snapshot work runs in the I/O worker.
    pub(crate) fn try_start_reclaimer_writeback_range(
        &self,
        start_index: usize,
        end_index: usize,
    ) -> Result<bool, SystemError> {
        let cache = self.upgrade()?;
        let domain_io = match cache.try_acquire_domain_io() {
            Ok(permit) => permit,
            Err(SystemError::ESTALE) => return Ok(false),
            Err(error) => return Err(error),
        };
        let Some(runner) = ReclaimerRunnerGuard::try_acquire(&cache) else {
            return Ok(false);
        };
        let Some(permit) = AsyncWritebackPermit::try_acquire() else {
            return Ok(false);
        };
        let inode = cache
            .inode()
            .and_then(|inode| inode.upgrade())
            .ok_or(SystemError::EIO)?;
        let manager = self.clone();
        let work_state = Mutex::new(Some((runner, permit, domain_io, cache, inode)));
        schedule_pagecache_io(Work::new(move || {
            let Some((runner, permit, domain_io, cache, inode)) = work_state.lock().take() else {
                return;
            };
            let _runner = runner;
            let _permit = permit;
            let _domain_io = domain_io;
            // Drain the caller's bounded scan range in file-offset order.
            // Keeping one runner per cache prevents overlapping reclaimer
            // workers, while advancing after each submitted batch avoids the
            // former one-batch-per-5-second throughput ceiling. A page dirtied
            // again behind the cursor is intentionally left for a later scan.
            let mut cursor = start_index;
            while cursor <= end_index {
                let claim = match manager.try_claim_next_batch_with_admission(
                    &cache,
                    &inode,
                    cursor,
                    end_index,
                    _domain_io.as_ref(),
                ) {
                    Ok(claim) => claim,
                    Err(_) => break,
                };
                let batch = match claim {
                    WritebackClaimOutcome::Claimed(batch) => batch,
                    WritebackClaimOutcome::NoBatch => break,
                    WritebackClaimOutcome::Deferred(progress) => {
                        // Reclaim must not wait, but it must not discard the
                        // producer edge either.  Re-enter this try-only path
                        // after progress so WRITE-only users are not reliant
                        // on an unrelated future reclaim round.
                        Self::schedule_reclaimer_deferred_retry(
                            progress,
                            manager.clone(),
                            start_index,
                            end_index,
                        );
                        break;
                    }
                    // The failure was already published and the claimed
                    // pages returned to Dirty; reclaim has no additional
                    // completion or errseq work to perform.
                    WritebackClaimOutcome::FailedRecorded(_) => break,
                };
                let Some(last_index) = batch.entries.last().map(|(index, _, _)| *index) else {
                    break;
                };
                match Self::submit_writeback_batch(batch) {
                    Ok(WritebackSubmitOutcome::Completed) if last_index != usize::MAX => {
                        cursor = last_index + 1;
                    }
                    Ok(WritebackSubmitOutcome::Completed) => break,
                    Ok(WritebackSubmitOutcome::Failed(_)) => break,
                    Ok(WritebackSubmitOutcome::Deferred(progress)) => {
                        Self::schedule_reclaimer_deferred_retry(
                            progress,
                            manager.clone(),
                            start_index,
                            end_index,
                        );
                        break;
                    }
                    Err(_) => break,
                }
            }
        }));
        Ok(true)
    }
}

impl PageCacheManager {
    pub fn writeback_page(&self, page_index: usize) -> Result<(), SystemError> {
        self.sync_data(page_index, page_index)
    }

    pub(super) fn wait_writeback_entry(entry: Arc<PageEntry>) -> Result<(), SystemError> {
        entry.wait_queue.wait_until(|| match entry.state() {
            PageState::Writeback => None,
            PageState::Error => Some(Err(SystemError::EIO)),
            _ => Some(Ok(())),
        })
    }

    fn wait_writeback_entry_incarnation(
        entry: Arc<PageEntry>,
        observed_incarnation: u64,
    ) -> Result<(), SystemError> {
        entry
            .wait_queue
            .wait_until(|| Self::writeback_incarnation_result(&entry, observed_incarnation))
    }

    pub(super) fn writeback_incarnation_result(
        entry: &PageEntry,
        observed_incarnation: u64,
    ) -> Option<Result<(), SystemError>> {
        if entry.writeback_incarnation.load(Ordering::Acquire) != observed_incarnation {
            return Some(Ok(()));
        }
        match entry.state() {
            PageState::Writeback => None,
            PageState::Error => Some(Err(SystemError::EIO)),
            _ => Some(Ok(())),
        }
    }

    fn finish_writeback_entry(
        cache: Arc<PageCache>,
        page_index: usize,
        entry: Arc<PageEntry>,
        page: Arc<Page>,
        result: Result<(), SystemError>,
    ) -> Result<(), SystemError> {
        Self::finish_writeback_entry_state(cache, page_index, entry, page, result, true)
    }

    pub(super) fn finish_writeback_entry_state(
        cache: Arc<PageCache>,
        page_index: usize,
        entry: Arc<PageEntry>,
        page: Arc<Page>,
        result: Result<(), SystemError>,
        record_error: bool,
    ) -> Result<(), SystemError> {
        let completed_incarnation = entry.writeback_incarnation.load(Ordering::Acquire);
        if let Err(e) = result {
            if record_error {
                cache.record_writeback_error_with_superblock(e.clone());
            }
            {
                let mut guard = page.write();
                guard.add_flags(PageFlags::PG_ERROR | PageFlags::PG_DIRTY);
            }
            {
                let mut inner = cache.inner.lock();
                let attached = inner
                    .pages
                    .get(&page_index)
                    .is_some_and(|current| Arc::ptr_eq(current, &entry));
                if attached {
                    entry.account_state_transition(PageState::Writeback, PageState::Dirty);
                    inner.writeback_pages.remove(&page_index);
                    inner.dirty_pages.insert(page_index);
                }
                entry.set_state(PageState::Dirty);
            }
            entry.wait_queue.wake_all();
            Self::dispatch_writeback_incarnation_retries(&cache, &entry, completed_incarnation);
            return Err(e);
        }

        {
            let mut guard = page.write();
            guard.remove_flags(PageFlags::PG_ERROR);
        }

        let page_dirty = page.read().flags().contains(PageFlags::PG_DIRTY);
        {
            let mut inner = cache.inner.lock();
            let attached = inner
                .pages
                .get(&page_index)
                .is_some_and(|current| Arc::ptr_eq(current, &entry));
            if !attached {
                entry.set_state(if page_dirty {
                    PageState::Dirty
                } else {
                    PageState::UpToDate
                });
                drop(inner);
                entry.wait_queue.wake_all();
                Self::dispatch_writeback_incarnation_retries(&cache, &entry, completed_incarnation);
                drop(cache.detach_dirty_retention_if_idle());
                return Ok(());
            }
            // `mark_page_dirty{,_prepared}()` publishes redirty through
            // `dirty_pages` while holding `inner`.  The PG_DIRTY sample above
            // must therefore be combined with that publication after taking
            // the same lock; otherwise a redirty registered between the
            // sample and this critical section would be overwritten as clean.
            // Do not read the page flags while holding `inner`: legacy
            // writeback paths acquire the page lock before updating the page
            // cache, so doing so would invert the established lock order.
            let redirtied = page_dirty || inner.dirty_pages.contains(&page_index);
            inner.writeback_pages.remove(&page_index);
            if redirtied {
                entry.account_state_transition(PageState::Writeback, PageState::Dirty);
                inner.dirty_pages.insert(page_index);
                entry.set_state(PageState::Dirty);
            } else {
                entry.account_state_transition(PageState::Writeback, PageState::UpToDate);
                inner.dirty_pages.remove(&page_index);
                entry.set_state(PageState::UpToDate);
            }
        }
        entry.wait_queue.wake_all();
        Self::dispatch_writeback_incarnation_retries(&cache, &entry, completed_incarnation);
        drop(cache.detach_dirty_retention_if_idle());
        Ok(())
    }
}
