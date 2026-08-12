use super::*;

static PAGECACHE_COMPLETION_SELFTEST_RUNNING: AtomicBool = AtomicBool::new(false);
static PAGECACHE_ACCOUNTING_SELFTEST_RUNNING: AtomicBool = AtomicBool::new(false);

// A batch large enough to dominate normal background noise verifies the
// page-cache VM counters, including the final-drop path that regressed. The
// tolerance avoids treating the global snapshot as an exact local oracle.
const PAGECACHE_ACCOUNTING_SELFTEST_WIRING_PAGES: usize = 128;
const PAGECACHE_ACCOUNTING_SELFTEST_WIRING_NOISE: i128 = 16;

struct PageCacheCompletionSelftestGuard;

impl Drop for PageCacheCompletionSelftestGuard {
    fn drop(&mut self) {
        PAGECACHE_COMPLETION_SELFTEST_RUNNING.store(false, Ordering::Release);
    }
}

struct PageCacheAccountingSelftestGuard;

impl Drop for PageCacheAccountingSelftestGuard {
    fn drop(&mut self) {
        PAGECACHE_ACCOUNTING_SELFTEST_RUNNING.store(false, Ordering::Release);
    }
}

#[inline(never)]
fn run_preallocated_batch_lifecycle_selftest() -> Result<bool, SystemError> {
    use crate::{
        arch::MMArch,
        mm::{
            allocator::page_frame::PageFrameCount,
            page::allocate_registered_intrinsic_unevictable_pages_exact, MemoryManagementArch as _,
        },
    };

    // Three pages exercises the allocator-rounded-tail path on the x86 buddy
    // allocator while the public batch must expose exactly the requested run.
    let batch = allocate_registered_intrinsic_unevictable_pages_exact(PageFrameCount::new(3))?;
    if batch.pages().len() != 3 {
        return Ok(false);
    }
    let start = batch.start_paddr();
    let addresses: Vec<_> = batch
        .pages()
        .iter()
        .map(|page| page.phys_address())
        .collect();
    if addresses
        .iter()
        .enumerate()
        .any(|(index, paddr)| paddr.data() != start.data() + index * MMArch::PAGE_SIZE)
    {
        return Ok(false);
    }
    if !batch
        .pages()
        .windows(2)
        .all(|pair| pair[0].shares_contiguous_frame_owner(&pair[1]))
    {
        return Ok(false);
    }
    let vaddr = unsafe { MMArch::phys_2_virt(start) }.ok_or(SystemError::EFAULT)?;
    let bytes =
        unsafe { core::slice::from_raw_parts(vaddr.data() as *const u8, 3 * MMArch::PAGE_SIZE) };
    if bytes.iter().any(|byte| *byte != 0) {
        return Ok(false);
    }
    for page in batch.pages() {
        page.write().add_flags(PageFlags::PG_UPTODATE);
    }

    let cache = PageCache::new(None, None);
    cache.adopt_preallocated_unevictable_batch(7, batch)?;
    if cache.inner.lock().pages_count() != 3
        || addresses
            .iter()
            .any(|paddr| !page_manager_lock().contains(paddr))
    {
        return Ok(false);
    }
    drop(cache);
    Ok(addresses
        .iter()
        .all(|paddr| !page_manager_lock().contains(paddr)))
}

struct PageCacheCompletionSelftestState {
    generic_started: AtomicUsize,
    generic_released: AtomicUsize,
    completion_done: AtomicBool,
    abort: AtomicBool,
    wait: WaitQueue,
}

/// Coordinates the one ordering which gives dirty incarnations their meaning:
/// an old writeback completion must not pass a front writer between that
/// writer setting PG_DIRTY and publishing its successor under `inner`.
struct PageCacheDirtyIncarnationRaceState {
    completion_started: AtomicBool,
    completion_done: AtomicBool,
    completion_succeeded: AtomicBool,
    wait: WaitQueue,
}

impl Default for PageCacheDirtyIncarnationRaceState {
    fn default() -> Self {
        Self {
            completion_started: AtomicBool::new(false),
            completion_done: AtomicBool::new(false),
            completion_succeeded: AtomicBool::new(false),
            wait: WaitQueue::default(),
        }
    }
}

impl PageCacheCompletionSelftestState {
    fn new() -> Self {
        Self {
            generic_started: AtomicUsize::new(0),
            generic_released: AtomicUsize::new(0),
            completion_done: AtomicBool::new(false),
            abort: AtomicBool::new(false),
            wait: WaitQueue::default(),
        }
    }

    fn release_waiters(&self) {
        self.abort.store(true, Ordering::Release);
        self.wait.wake_all();
    }
}

/// Synchronization state for the deterministic file-fault invalidation retry
/// selftest. The participants model the real wait graph: snapshot holds
/// invalidate-read and waits for MM-read; a buffered writer queues
/// invalidate-write; a file fault observes retry while MM-write is held; its
/// outer retry waiter runs only after that MM-write guard is gone.
struct PageCacheInvalidateRetrySelftestState {
    snapshot_holds_invalidate: AtomicBool,
    snapshot_acquired_mm_read: AtomicBool,
    buffered_writer_queued: AtomicBool,
    buffered_writer_acquired: AtomicBool,
    fault_probe_observed_retry: AtomicBool,
    fault_observed_retry: AtomicBool,
    fault_waiter_finished: AtomicBool,
    stop_probe: AtomicBool,
    wait: WaitQueue,
}

impl PageCacheInvalidateRetrySelftestState {
    fn new() -> Self {
        Self {
            snapshot_holds_invalidate: AtomicBool::new(false),
            snapshot_acquired_mm_read: AtomicBool::new(false),
            buffered_writer_queued: AtomicBool::new(false),
            buffered_writer_acquired: AtomicBool::new(false),
            fault_probe_observed_retry: AtomicBool::new(false),
            fault_observed_retry: AtomicBool::new(false),
            fault_waiter_finished: AtomicBool::new(false),
            stop_probe: AtomicBool::new(false),
            wait: WaitQueue::default(),
        }
    }
}

struct PageCacheTagScanChunkSelftestState {
    start_reader: AtomicBool,
    reader_attempting: AtomicBool,
    reader_acquired: AtomicBool,
    reader_saw_unscanned_tail: AtomicBool,
    stop: AtomicBool,
    wait: WaitQueue,
}

impl Default for PageCacheTagScanChunkSelftestState {
    fn default() -> Self {
        Self {
            start_reader: AtomicBool::new(false),
            reader_attempting: AtomicBool::new(false),
            reader_acquired: AtomicBool::new(false),
            reader_saw_unscanned_tail: AtomicBool::new(false),
            stop: AtomicBool::new(false),
            wait: WaitQueue::default(),
        }
    }
}

#[derive(Debug, Default)]
struct PageCacheQuotaSelftestBackend {
    reserved: AtomicUsize,
    released: AtomicUsize,
}

/// Test-only backend for the writeback claim lock-order contract.
struct PageCacheAdmissionOrderSelftestBackend {
    cache: SpinLock<Weak<PageCache>>,
    order: PageCacheWritebackAdmissionOrder,
    expects_invalidate_read: bool,
    observed_expected_order: AtomicUsize,
    observed_unexpected_order: AtomicUsize,
}

impl PageCacheAdmissionOrderSelftestBackend {
    fn new(order: PageCacheWritebackAdmissionOrder, expects_invalidate_read: bool) -> Self {
        Self {
            cache: SpinLock::new(Weak::new()),
            order,
            expects_invalidate_read,
            observed_expected_order: AtomicUsize::new(0),
            observed_unexpected_order: AtomicUsize::new(0),
        }
    }

    fn observe_invalidate_order(&self) -> Result<(), SystemError> {
        let cache = self.cache.lock().upgrade().ok_or(SystemError::EIO)?;
        let invalidate_read_held = cache.invalidate_lock.try_write().is_none();
        if invalidate_read_held == self.expects_invalidate_read {
            self.observed_expected_order.fetch_add(1, Ordering::Relaxed);
        } else {
            self.observed_unexpected_order
                .fetch_add(1, Ordering::Relaxed);
        }
        Ok(())
    }
}

impl core::fmt::Debug for PageCacheAdmissionOrderSelftestBackend {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PageCacheAdmissionOrderSelftestBackend")
            .finish_non_exhaustive()
    }
}

impl PageCacheBackend for PageCacheAdmissionOrderSelftestBackend {
    fn read_page(&self, _index: usize, _buf: &mut [u8]) -> Result<usize, SystemError> {
        Ok(0)
    }

    fn write_page(&self, _index: usize, buf: &[u8]) -> Result<usize, SystemError> {
        Ok(buf.len())
    }

    fn npages(&self) -> usize {
        0
    }

    fn writeback_admission_order(&self) -> PageCacheWritebackAdmissionOrder {
        self.order
    }

    fn with_write_admission(
        &self,
        claim: &mut dyn FnMut() -> Result<(), SystemError>,
    ) -> Result<(), SystemError> {
        self.observe_invalidate_order()?;
        claim()
    }

    fn try_with_write_admission(
        &self,
        claim: &mut dyn FnMut() -> Result<(), SystemError>,
    ) -> Result<bool, SystemError> {
        self.observe_invalidate_order()?;
        claim()?;
        Ok(true)
    }
}

/// State shared by a synthetic token backend used to exercise the production
/// bind -> snapshot -> submit/cancel lifecycle without coupling that lifecycle
/// to any filesystem implementation.
type PageCacheWritebackProgressCallback =
    Arc<dyn Fn(PageCacheWritebackProgressOutcome) + Send + Sync>;

struct PageCacheSubmissionSelftestState {
    bind_attempts: AtomicUsize,
    bound: AtomicUsize,
    submitted: AtomicUsize,
    failed_submissions: AtomicUsize,
    submitted_after_admission: AtomicUsize,
    submitted_while_admitted: AtomicUsize,
    cancelled: AtomicUsize,
    cancelled_while_admitted: AtomicUsize,
    cancelled_outside_admission: AtomicUsize,
    cancelled_after_admission: AtomicUsize,
    cancelled_after_admission_without_invalidate: AtomicUsize,
    cancellation_reacquired_admission: AtomicUsize,
    snapshotted_after_admission: AtomicUsize,
    snapshotted_while_admitted: AtomicUsize,
    bound_outside_admission: AtomicUsize,
    fallback_writes: AtomicUsize,
    private_claims: AtomicUsize,
    admission_depth: AtomicUsize,
    first_index: AtomicUsize,
    last_index: AtomicUsize,
    file_size: AtomicUsize,
    valid_bytes: AtomicUsize,
    last_writeback_generation: AtomicU64,
    generation_regressions: AtomicUsize,
    single_page_certificates: AtomicUsize,
    certificate_errors: AtomicUsize,
    fail_next_bind: AtomicBool,
    fail_next_submit: AtomicBool,
    fail_admission_after_claim: AtomicBool,
    defer_next_bind: AtomicBool,
    defer_next_submit: AtomicBool,
    deferred_before_claim: AtomicUsize,
    deferred_after_submit: AtomicUsize,
    deferred_waits: AtomicUsize,
    deferred_waiters_entered: AtomicUsize,
    progress_sequence: AtomicUsize,
    progress_wait: WaitQueue,
    retry_callbacks: Mutex<Vec<PageCacheWritebackProgressCallback>>,
}

impl core::fmt::Debug for PageCacheSubmissionSelftestState {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PageCacheSubmissionSelftestState")
            .field("bind_attempts", &self.bind_attempts.load(Ordering::Acquire))
            .field("submitted", &self.submitted.load(Ordering::Acquire))
            .finish_non_exhaustive()
    }
}

impl Default for PageCacheSubmissionSelftestState {
    fn default() -> Self {
        Self {
            bind_attempts: AtomicUsize::new(0),
            bound: AtomicUsize::new(0),
            submitted: AtomicUsize::new(0),
            failed_submissions: AtomicUsize::new(0),
            submitted_after_admission: AtomicUsize::new(0),
            submitted_while_admitted: AtomicUsize::new(0),
            cancelled: AtomicUsize::new(0),
            cancelled_while_admitted: AtomicUsize::new(0),
            cancelled_outside_admission: AtomicUsize::new(0),
            cancelled_after_admission: AtomicUsize::new(0),
            cancelled_after_admission_without_invalidate: AtomicUsize::new(0),
            cancellation_reacquired_admission: AtomicUsize::new(0),
            snapshotted_after_admission: AtomicUsize::new(0),
            snapshotted_while_admitted: AtomicUsize::new(0),
            bound_outside_admission: AtomicUsize::new(0),
            fallback_writes: AtomicUsize::new(0),
            private_claims: AtomicUsize::new(0),
            admission_depth: AtomicUsize::new(0),
            first_index: AtomicUsize::new(0),
            last_index: AtomicUsize::new(0),
            file_size: AtomicUsize::new(0),
            valid_bytes: AtomicUsize::new(0),
            last_writeback_generation: AtomicU64::new(0),
            generation_regressions: AtomicUsize::new(0),
            single_page_certificates: AtomicUsize::new(0),
            certificate_errors: AtomicUsize::new(0),
            fail_next_bind: AtomicBool::new(false),
            fail_next_submit: AtomicBool::new(false),
            fail_admission_after_claim: AtomicBool::new(false),
            defer_next_bind: AtomicBool::new(false),
            defer_next_submit: AtomicBool::new(false),
            deferred_before_claim: AtomicUsize::new(0),
            deferred_after_submit: AtomicUsize::new(0),
            deferred_waits: AtomicUsize::new(0),
            deferred_waiters_entered: AtomicUsize::new(0),
            progress_sequence: AtomicUsize::new(0),
            progress_wait: WaitQueue::default(),
            retry_callbacks: Mutex::new(Vec::new()),
        }
    }
}

#[derive(Debug)]
struct PageCacheSubmissionSelftestBackend {
    state: Arc<PageCacheSubmissionSelftestState>,
    admission_order: PageCacheWritebackAdmissionOrder,
    snapshot_phase: PageCacheWritebackSnapshotPhase,
}

#[derive(Debug)]
struct PageCacheSubmissionSelftestToken {
    state: Arc<PageCacheSubmissionSelftestState>,
    writeback_generation: u64,
    resolved: bool,
}

/// Synthetic progress source used to prove that both PageCache defer paths
/// wait for a concrete producer event instead of spinning on Dirty pages.
#[derive(Debug)]
struct PageCacheSubmissionSelftestProgress {
    state: Arc<PageCacheSubmissionSelftestState>,
    observed_sequence: usize,
}

/// Gate the exact tagged claim -> snapshot hand-off which a WAIT_BEFORE
/// waiter observes. The debugfs selftest uses it to prove that clearing a
/// frozen Dirty tag is never visible before the replacement submission record
/// has been published.
struct PageCacheTaggedSubmissionSelftestGate {
    snapshot_entered: AtomicBool,
    release_snapshot: AtomicBool,
    waiter_started: AtomicBool,
    waiter_finished: AtomicBool,
    worker_finished: AtomicBool,
    worker_succeeded: AtomicBool,
    wait: WaitQueue,
}

impl Default for PageCacheTaggedSubmissionSelftestGate {
    fn default() -> Self {
        Self {
            snapshot_entered: AtomicBool::new(false),
            release_snapshot: AtomicBool::new(false),
            waiter_started: AtomicBool::new(false),
            waiter_finished: AtomicBool::new(false),
            worker_finished: AtomicBool::new(false),
            worker_succeeded: AtomicBool::new(false),
            wait: WaitQueue::default(),
        }
    }
}

impl PageCacheSubmissionSelftestState {
    fn deferred_progress(self: &Arc<Self>) -> Arc<dyn PageCacheWritebackProgress> {
        let observed_sequence = self.progress_sequence.load(Ordering::Acquire);
        Arc::new(PageCacheSubmissionSelftestProgress {
            state: self.clone(),
            observed_sequence,
        })
    }

    fn release_deferred_progress(&self) {
        // Publish the producer transition and detach its continuations under
        // one lock.  Otherwise a retry can observe the old sequence after we
        // have taken the list, append itself to the now-detached list, and
        // miss this progress forever.
        let callbacks = {
            let mut callbacks = self.retry_callbacks.lock();
            self.progress_sequence.fetch_add(1, Ordering::AcqRel);
            mem::take(&mut *callbacks)
        };
        self.progress_wait.wake_all();
        for callback in callbacks {
            callback(PageCacheWritebackProgressOutcome::Progress);
        }
    }
}

impl PageCacheWritebackProgress for PageCacheSubmissionSelftestProgress {
    fn wait_for_progress(&self) -> PageCacheWritebackProgressOutcome {
        self.state.deferred_waits.fetch_add(1, Ordering::Relaxed);
        self.state
            .deferred_waiters_entered
            .fetch_add(1, Ordering::Release);
        self.state.progress_wait.wake_all();
        self.state.progress_wait.wait_until(|| {
            (self.state.progress_sequence.load(Ordering::Acquire) != self.observed_sequence)
                .then_some(())
        });
        PageCacheWritebackProgressOutcome::Progress
    }

    fn register_retry(&self, retry: Arc<dyn Fn(PageCacheWritebackProgressOutcome) + Send + Sync>) {
        let invoke_now = {
            let mut callbacks = self.state.retry_callbacks.lock();
            if self.state.progress_sequence.load(Ordering::Acquire) == self.observed_sequence {
                callbacks.push(retry.clone());
                false
            } else {
                true
            }
        };
        if invoke_now {
            retry(PageCacheWritebackProgressOutcome::Progress);
        }
    }
}

impl Drop for PageCacheSubmissionSelftestToken {
    fn drop(&mut self) {
        assert!(
            self.resolved,
            "PageCache submission token was dropped without submit or cancel"
        );
    }
}

impl PageCacheWritebackSubmission for PageCacheSubmissionSelftestToken {
    fn submit(
        mut self: Box<Self>,
        descriptor: &PageCacheWritebackDescriptor,
        data: &[u8],
    ) -> Result<PageCacheWritebackSubmitResult, SystemError> {
        if self.state.admission_depth.load(Ordering::Acquire) == 0 {
            self.state
                .submitted_after_admission
                .fetch_add(1, Ordering::Relaxed);
        } else {
            self.state
                .submitted_while_admitted
                .fetch_add(1, Ordering::Relaxed);
        }
        self.state
            .first_index
            .store(descriptor.first_index(), Ordering::Release);
        self.state
            .last_index
            .store(descriptor.last_index(), Ordering::Release);
        self.state
            .file_size
            .store(descriptor.file_size(), Ordering::Release);
        self.state
            .valid_bytes
            .store(descriptor.valid_bytes(), Ordering::Release);
        if descriptor.valid_bytes() != data.len()
            || descriptor.writeback_generation() != self.writeback_generation
        {
            let previous = self.state.private_claims.fetch_sub(1, Ordering::AcqRel);
            assert_eq!(previous, 1, "submission token private claim underflow");
            self.resolved = true;
            return Err(SystemError::EIO);
        }
        if self.state.defer_next_submit.swap(false, Ordering::AcqRel) {
            self.state
                .deferred_after_submit
                .fetch_add(1, Ordering::Relaxed);
            let progress = self.state.deferred_progress();
            let previous = self.state.private_claims.fetch_sub(1, Ordering::AcqRel);
            assert_eq!(previous, 1, "submission token private claim underflow");
            self.resolved = true;
            return Ok(PageCacheWritebackSubmitResult::Deferred(progress));
        }
        if self.state.fail_next_submit.swap(false, Ordering::AcqRel) {
            self.state
                .failed_submissions
                .fetch_add(1, Ordering::Relaxed);
            let previous = self.state.private_claims.fetch_sub(1, Ordering::AcqRel);
            assert_eq!(previous, 1, "submission token private claim underflow");
            self.resolved = true;
            return Err(SystemError::EIO);
        }
        self.state.submitted.fetch_add(1, Ordering::Relaxed);
        let previous = self.state.private_claims.fetch_sub(1, Ordering::AcqRel);
        assert_eq!(previous, 1, "submission token private claim underflow");
        self.resolved = true;
        Ok(PageCacheWritebackSubmitResult::Completed)
    }

    fn cancel(mut self: Box<Self>, context: PageCacheWritebackCancellationContext) {
        self.state.cancelled.fetch_add(1, Ordering::Relaxed);
        match context {
            PageCacheWritebackCancellationContext::BeforeSubmitWithAdmission => {
                if self.state.admission_depth.load(Ordering::Acquire) == 1 {
                    self.state
                        .cancelled_while_admitted
                        .fetch_add(1, Ordering::Relaxed);
                } else {
                    self.state
                        .cancelled_outside_admission
                        .fetch_add(1, Ordering::Relaxed);
                }
            }
            PageCacheWritebackCancellationContext::AfterAdmissionWithInvalidateRead => {
                self.state
                    .cancelled_after_admission
                    .fetch_add(1, Ordering::Relaxed);
                // A delayed-allocation finalizer may need to reacquire its
                // private size -> I/O admission after PageCache has dropped
                // the original admission but while invalidate-read still
                // protects the batch. Make that transition explicit so a
                // regression to another cancellation context cannot pass by
                // merely decrementing the same test counter.
                let previous = self.state.admission_depth.fetch_add(1, Ordering::AcqRel);
                assert_eq!(
                    previous, 0,
                    "post-admission cancellation ran while backend admission was still held"
                );
                self.state
                    .cancellation_reacquired_admission
                    .fetch_add(1, Ordering::Relaxed);
                let previous = self.state.admission_depth.fetch_sub(1, Ordering::AcqRel);
                assert_eq!(
                    previous, 1,
                    "post-admission cancellation admission depth underflow"
                );
            }
            PageCacheWritebackCancellationContext::AfterAdmissionWithoutInvalidateRead => {
                assert_eq!(
                    self.state.admission_depth.load(Ordering::Acquire),
                    0,
                    "legacy post-admission cancellation retained backend admission"
                );
                self.state
                    .cancelled_after_admission_without_invalidate
                    .fetch_add(1, Ordering::Relaxed);
            }
        }
        let previous = self.state.private_claims.fetch_sub(1, Ordering::AcqRel);
        assert_eq!(previous, 1, "submission token private claim underflow");
        self.resolved = true;
    }
}

impl PageCacheBackend for PageCacheSubmissionSelftestBackend {
    fn read_page(&self, _index: usize, _buf: &mut [u8]) -> Result<usize, SystemError> {
        Ok(0)
    }

    fn write_page(&self, _index: usize, _buf: &[u8]) -> Result<usize, SystemError> {
        self.state.fallback_writes.fetch_add(1, Ordering::Relaxed);
        Err(SystemError::EIO)
    }

    fn npages(&self) -> usize {
        0
    }

    fn writeback_admission_order(&self) -> PageCacheWritebackAdmissionOrder {
        self.admission_order
    }

    fn writeback_snapshot_phase(&self) -> PageCacheWritebackSnapshotPhase {
        self.snapshot_phase
    }

    fn writeback_submission_protocol(&self) -> PageCacheWritebackProtocol {
        PageCacheWritebackProtocol::Token
    }

    fn write_batch_pages(&self) -> Result<usize, SystemError> {
        Ok(2)
    }

    fn with_write_admission(
        &self,
        claim: &mut dyn FnMut() -> Result<(), SystemError>,
    ) -> Result<(), SystemError> {
        let previous = self.state.admission_depth.fetch_add(1, Ordering::AcqRel);
        assert_eq!(
            previous, 0,
            "submission selftest admission unexpectedly nested"
        );
        let result = claim();
        let previous = self.state.admission_depth.fetch_sub(1, Ordering::AcqRel);
        assert_eq!(previous, 1, "submission selftest admission depth underflow");
        if result.is_ok()
            && self
                .state
                .fail_admission_after_claim
                .swap(false, Ordering::AcqRel)
        {
            return Err(SystemError::EIO);
        }
        result
    }

    fn try_with_write_admission(
        &self,
        claim: &mut dyn FnMut() -> Result<(), SystemError>,
    ) -> Result<bool, SystemError> {
        self.with_write_admission(claim)?;
        Ok(true)
    }

    fn bind_writeback_submission(
        &self,
        descriptor: &PageCacheWritebackDescriptor,
    ) -> Result<PageCacheWritebackBindResult, SystemError> {
        self.state.bind_attempts.fetch_add(1, Ordering::Relaxed);
        if self.state.admission_depth.load(Ordering::Acquire) != 1 {
            self.state
                .bound_outside_admission
                .fetch_add(1, Ordering::Relaxed);
        }
        self.state.bound.fetch_add(1, Ordering::Relaxed);
        let previous_generation = self
            .state
            .last_writeback_generation
            .swap(descriptor.writeback_generation(), Ordering::AcqRel);
        if descriptor.writeback_generation() == 0
            || (previous_generation != 0
                && descriptor.writeback_generation() <= previous_generation)
        {
            self.state
                .generation_regressions
                .fetch_add(1, Ordering::Relaxed);
        }
        let certificates = descriptor.dirty_certificates();
        let expected_count = descriptor
            .last_index()
            .checked_sub(descriptor.first_index())
            .and_then(|distance| distance.checked_add(1));
        let certificates_valid = expected_count == Some(certificates.len())
            && certificates
                .iter()
                .enumerate()
                .all(|(offset, certificate)| {
                    descriptor
                        .first_index()
                        .checked_add(offset)
                        .is_some_and(|index| certificate.page_index() == index)
                        && certificate.cache_instance_id() != 0
                        && certificate.entry_instance_id() != 0
                        && certificate.dirty_incarnation() != 0
                        && matches!(
                            certificate.kind(),
                            PageCacheDirtyTransitionKind::NewlyDirty
                                | PageCacheDirtyTransitionKind::RedirtiedDuringWriteback
                        )
                });
        if !certificates_valid {
            self.state
                .certificate_errors
                .fetch_add(1, Ordering::Relaxed);
        } else if certificates.len() == 1 {
            self.state
                .single_page_certificates
                .fetch_add(1, Ordering::Relaxed);
        }
        if self.state.defer_next_bind.swap(false, Ordering::AcqRel) {
            self.state
                .deferred_before_claim
                .fetch_add(1, Ordering::Relaxed);
            return Ok(PageCacheWritebackBindResult::Deferred(
                self.state.deferred_progress(),
            ));
        }
        let previous = self.state.private_claims.fetch_add(1, Ordering::AcqRel);
        assert_eq!(previous, 0, "submission selftest private claim leaked");
        if self.state.fail_next_bind.swap(false, Ordering::AcqRel) {
            let previous = self.state.private_claims.fetch_sub(1, Ordering::AcqRel);
            assert_eq!(previous, 1, "submission selftest private claim underflow");
            return Err(SystemError::EIO);
        }
        self.state
            .valid_bytes
            .store(descriptor.valid_bytes(), Ordering::Release);
        Ok(PageCacheWritebackBindResult::Submission(Box::new(
            PageCacheSubmissionSelftestToken {
                state: self.state.clone(),
                writeback_generation: descriptor.writeback_generation(),
                resolved: false,
            },
        )))
    }
}

impl PageCacheBackend for PageCacheQuotaSelftestBackend {
    fn read_page(&self, _index: usize, _buf: &mut [u8]) -> Result<usize, SystemError> {
        Ok(0)
    }

    fn write_page(&self, _index: usize, buf: &[u8]) -> Result<usize, SystemError> {
        Ok(buf.len())
    }

    fn npages(&self) -> usize {
        0
    }

    fn reserve_page(&self) -> Result<(), SystemError> {
        self.reserved.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    fn release_page(&self) {
        self.released.fetch_add(1, Ordering::Relaxed);
    }
}

/// Verify that terminal writeback completion cannot be stranded behind generic
/// page-cache workers waiting for that completion.
///
/// This is only called from a root-readable debugfs selftest. It injects no
/// delay or branch into normal page-cache operation.
pub(crate) fn run_completion_domain_debug_selftest() -> Result<alloc::string::String, SystemError> {
    if PAGECACHE_COMPLETION_SELFTEST_RUNNING
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return Err(SystemError::EBUSY);
    }
    let _running = PageCacheCompletionSelftestGuard;
    let state = Arc::new(PageCacheCompletionSelftestState::new());

    // Place exactly one waiter on every generic page-cache worker. Direct queue
    // selection keeps the test deterministic even if unrelated page-cache work
    // advances the production round-robin counter concurrently. The waiters
    // model host invalidation after it has observed a published Writeback page.
    for workqueue in PAGECACHE_IO_WQS.iter() {
        let waiter_state = state.clone();
        workqueue.enqueue(Work::new(move || {
            waiter_state.generic_started.fetch_add(1, Ordering::AcqRel);
            waiter_state.wait.wake_all();
            waiter_state.wait.wait_until(|| {
                (waiter_state.completion_done.load(Ordering::Acquire)
                    || waiter_state.abort.load(Ordering::Acquire))
                .then_some(())
            });
            waiter_state.generic_released.fetch_add(1, Ordering::AcqRel);
            waiter_state.wait.wake_all();
        }));
    }

    const SELFTEST_TIMEOUT: Duration = Duration::from_secs(2);
    if let Err(error) = state.wait.wait_until_timeout(
        || (state.generic_started.load(Ordering::Acquire) == PAGECACHE_IO_WORKERS).then_some(()),
        SELFTEST_TIMEOUT,
    ) {
        state.release_waiters();
        return Ok(alloc::format!(
            "status=fail stage=occupy_generic error={error:?} started={} expected={}\n",
            state.generic_started.load(Ordering::Acquire),
            PAGECACHE_IO_WORKERS
        ));
    }

    let completion_state = state.clone();
    PAGECACHE_WRITEBACK_WQS[0].enqueue(Work::new(move || {
        completion_state
            .completion_done
            .store(true, Ordering::Release);
        completion_state.wait.wake_all();
    }));

    let completion_result = state.wait.wait_until_timeout(
        || state.completion_done.load(Ordering::Acquire).then_some(()),
        SELFTEST_TIMEOUT,
    );
    if completion_result.is_err() {
        state.release_waiters();
    }
    let released_result = state.wait.wait_until_timeout(
        || (state.generic_released.load(Ordering::Acquire) == PAGECACHE_IO_WORKERS).then_some(()),
        SELFTEST_TIMEOUT,
    );
    state.release_waiters();

    if let Err(error) = completion_result {
        return Ok(alloc::format!(
            "status=fail stage=completion error={error:?} released={} expected={}\n",
            state.generic_released.load(Ordering::Acquire),
            PAGECACHE_IO_WORKERS
        ));
    }
    if let Err(error) = released_result {
        return Ok(alloc::format!(
            "status=fail stage=release error={error:?} released={} expected={}\n",
            state.generic_released.load(Ordering::Acquire),
            PAGECACHE_IO_WORKERS
        ));
    }

    Ok(alloc::format!(
        "status=ok generic_waiters={} completion_domain=independent\n",
        PAGECACHE_IO_WORKERS
    ))
}

/// Prove the lock-order progress rule used by `do_shared_fault()` and
/// `do_cow_fault()` without relying on scheduler timing.
///
/// The selftest deliberately creates the former three-way cycle. It uses the
/// production PageCache invalidation semaphore and the production
/// `PageCacheInvalidateRetryWait`; the separate RwSem is a precise stand-in
/// for `AddressSpace::write/read`, allowing the test to pin every edge before
/// releasing the fault side. No production PageCache path observes this test.
fn run_invalidate_retry_lock_order_selftest() -> Result<bool, SystemError> {
    const SELFTEST_TIMEOUT: Duration = Duration::from_secs(2);

    let cache = PageCache::new(None, None);
    let mm = Arc::new(RwSem::new(()));
    let state = Arc::new(PageCacheInvalidateRetrySelftestState::new());

    // B: the simulated file fault owns MM-write before it reaches the
    // PageCache invalidation edge.
    let mm_write = mm.write();

    // A: snapshot/writeback owns invalidate-read, then waits for MM-read.
    let snapshot_cache = cache.clone();
    let snapshot_mm = mm.clone();
    let snapshot_state = state.clone();
    PAGECACHE_IO_WQS[0].enqueue(Work::new(move || {
        let _invalidate = snapshot_cache.invalidate_read();
        snapshot_state
            .snapshot_holds_invalidate
            .store(true, Ordering::Release);
        snapshot_state.wait.wake_all();
        let _mm_read = snapshot_mm.read();
        snapshot_state
            .snapshot_acquired_mm_read
            .store(true, Ordering::Release);
        snapshot_state.wait.wake_all();
    }));

    let retry_wait = (|| -> Result<Arc<dyn FaultRetryWait>, SystemError> {
        state.wait.wait_until_timeout(
            || {
                state
                    .snapshot_holds_invalidate
                    .load(Ordering::Acquire)
                    .then_some(())
            },
            SELFTEST_TIMEOUT,
        )?;

        // C: a buffered write queues invalidate-write behind A. RwSem writer
        // preference then makes the file fault's try-read fail deterministically.
        let writer_cache = cache.clone();
        let writer_state = state.clone();
        PAGECACHE_IO_WQS[1].enqueue(Work::new(move || {
            writer_state
                .buffered_writer_queued
                .store(true, Ordering::Release);
            writer_state.wait.wake_all();
            let _invalidate = writer_cache.invalidate_write();
            writer_state
                .buffered_writer_acquired
                .store(true, Ordering::Release);
            writer_state.wait.wake_all();
        }));

        state.wait.wait_until_timeout(
            || {
                state
                    .buffered_writer_queued
                    .load(Ordering::Acquire)
                    .then_some(())
            },
            SELFTEST_TIMEOUT,
        )?;

        // `invalidate_write()` registers its writer waiter after the worker
        // announces that it is about to block.  A separate probe therefore
        // observes the *actual* writer-preference condition and wakes this
        // test only then; waiting directly on `state.wait` here could miss
        // that registration edge and time out merely due to scheduling.
        let probe_cache = cache.clone();
        let probe_state = state.clone();
        PAGECACHE_IO_WQS[2].enqueue(Work::new(move || {
            while !probe_state.stop_probe.load(Ordering::Acquire) {
                if let PageCacheFaultInvalidateRead::Retry(_) =
                    probe_cache.file_fault_invalidate_read()
                {
                    probe_state
                        .fault_probe_observed_retry
                        .store(true, Ordering::Release);
                    probe_state.wait.wake_all();
                    return;
                }
                crate::sched::sched_yield();
            }
            probe_state.wait.wake_all();
        }));
        state.wait.wait_until_timeout(
            || {
                state
                    .fault_probe_observed_retry
                    .load(Ordering::Acquire)
                    .then_some(())
            },
            SELFTEST_TIMEOUT,
        )?;

        // The helper is the same decision point used by both actual file
        // fault branches. Constructing its retry token is nonblocking and is
        // intentionally done while the simulated MM-write guard is still
        // held; only wait() is allowed after that guard is released.
        match cache.file_fault_invalidate_read() {
            PageCacheFaultInvalidateRead::Retry(retry_wait) => {
                state.fault_observed_retry.store(true, Ordering::Release);
                Ok(retry_wait)
            }
            PageCacheFaultInvalidateRead::Acquired(_guard) => {
                Err(SystemError::EAGAIN_OR_EWOULDBLOCK)
            }
        }
    })();

    // This is the required outer-fault retry boundary. Releasing MM-write
    // lets A finish; C then owns and releases invalidate-write; only then may
    // the production retry waiter obtain invalidate-read and finish.
    drop(mm_write);
    let retry_wait = match retry_wait {
        Ok(retry_wait) => retry_wait,
        Err(err) => {
            // A failed setup must never strand snapshot/writeback workers
            // behind the test's simulated MM write lock.
            state.stop_probe.store(true, Ordering::Release);
            state.wait.wake_all();
            return Err(err);
        }
    };
    let waiter_state = state.clone();
    PAGECACHE_IO_WQS[3].enqueue(Work::new(move || {
        if retry_wait.wait().is_ok() {
            waiter_state
                .fault_waiter_finished
                .store(true, Ordering::Release);
        }
        waiter_state.wait.wake_all();
    }));

    let completed = state.wait.wait_until_timeout(
        || {
            (state.snapshot_acquired_mm_read.load(Ordering::Acquire)
                && state.buffered_writer_acquired.load(Ordering::Acquire)
                && state.fault_waiter_finished.load(Ordering::Acquire))
            .then_some(())
        },
        SELFTEST_TIMEOUT,
    );
    state.stop_probe.store(true, Ordering::Release);
    completed?;

    Ok(state.fault_observed_retry.load(Ordering::Acquire)
        && state.snapshot_holds_invalidate.load(Ordering::Acquire)
        && state.snapshot_acquired_mm_read.load(Ordering::Acquire)
        && state.buffered_writer_queued.load(Ordering::Acquire)
        && state.buffered_writer_acquired.load(Ordering::Acquire)
        && state.fault_waiter_finished.load(Ordering::Acquire))
}

/// Verify that a large TOWRITE-style tag pass releases mapping exclusion
/// between bounded chunks. The freeze callback starts a reader while it still
/// owns the initial writer guard; that reader can therefore acquire only at a
/// subsequent chunk boundary, where it records whether the final page is
/// still untagged.
fn run_tag_scan_chunk_release_selftest() -> Result<bool, SystemError> {
    const SELFTEST_PAGES: usize = 513;
    const SELFTEST_TIMEOUT: Duration = Duration::from_secs(2);

    let cache = PageCache::new(None, None);
    let mut pages = Vec::with_capacity(SELFTEST_PAGES);
    for index in 0..SELFTEST_PAGES {
        let page = cache.get_or_create_page_zero(index)?;
        page.write().add_flags(PageFlags::PG_DIRTY);
        let mut inner = cache.inner.lock();
        let entry = inner.get_entry(index).ok_or(SystemError::EIO)?;
        entry.account_state_transition(PageState::UpToDate, PageState::Dirty);
        entry.set_state(PageState::Dirty);
        inner.dirty_pages.insert(index);
        drop(inner);
        pages.push(page);
    }

    let state = Arc::new(PageCacheTagScanChunkSelftestState::default());
    let worker_cache = cache.clone();
    let worker_state = state.clone();
    PAGECACHE_IO_WQS[0].enqueue(Work::new(move || {
        let started = worker_state.wait.wait_until_timeout(
            || {
                (worker_state.start_reader.load(Ordering::Acquire)
                    || worker_state.stop.load(Ordering::Acquire))
                .then_some(())
            },
            SELFTEST_TIMEOUT,
        );
        if started.is_ok() && !worker_state.stop.load(Ordering::Acquire) {
            worker_state
                .reader_attempting
                .store(true, Ordering::Release);
            worker_state.wait.wake_all();
            let _invalidate = worker_cache.invalidate_read();
            let tail_unscanned = {
                let inner = worker_cache.inner.lock();
                inner
                    .pages
                    .get(&(SELFTEST_PAGES - 1))
                    .is_some_and(|entry| entry.writeback_tag() == 0)
            };
            worker_state
                .reader_saw_unscanned_tail
                .store(tail_unscanned, Ordering::Release);
            worker_state.reader_acquired.store(true, Ordering::Release);
        }
        worker_state.wait.wake_all();
    }));

    // A synthetic cache has no inode, so dispatch returns EIO after the tag
    // scan and retires the generation. The test concerns only the preceding
    // mapping-exclusion window.
    let _ = cache
        .manager
        .start_writeback_range_with_freeze(0, SELFTEST_PAGES - 1, || {
            state.start_reader.store(true, Ordering::Release);
            state.wait.wake_all();
            state.wait.wait_until_timeout(
                || {
                    state
                        .reader_attempting
                        .load(Ordering::Acquire)
                        .then_some(())
                },
                SELFTEST_TIMEOUT,
            )?;
            // Let the worker enter the production RwSem read path while this
            // callback's caller still owns the initial write guard.
            crate::sched::sched_yield();
            Ok(())
        });
    let observed = state.wait.wait_until_timeout(
        || {
            state
                .reader_acquired
                .load(Ordering::Acquire)
                .then_some(state.reader_saw_unscanned_tail.load(Ordering::Acquire))
        },
        SELFTEST_TIMEOUT,
    );
    state.stop.store(true, Ordering::Release);
    state.wait.wake_all();

    for (index, page) in pages.into_iter().enumerate() {
        let _ = cache.manager.remove_page(index)?;
        let paddr = page.phys_address();
        page_manager_lock().remove_page(&paddr);
        let _ = page_reclaimer_lock().remove_page(&paddr);
    }

    observed
}

/// Exercise page-cache membership accounting with local identity assertions and
/// a high-signal aggregate check of the production vmstat wiring.
fn run_dirty_incarnation_selftest() -> Result<bool, SystemError> {
    let cache = PageCache::new_shmem(None, None);
    let page = cache.get_or_create_page_zero(0)?;
    let first = {
        let mut page_locked = page.write();
        page_locked.add_flags(PageFlags::PG_DIRTY);
        match cache
            .mark_page_dirty_page_locked_with_transition(0, &page_locked)?
            .ok_or(SystemError::EIO)?
        {
            PageCacheDirtyTransition::Started(incarnation) => incarnation,
            PageCacheDirtyTransition::Merged(_) => return Ok(false),
        }
    };
    let first_ok = first.kind() == PageCacheDirtyTransitionKind::NewlyDirty
        && first.page_index() == 0
        && first.dirty_incarnation() != 0
        && cache.validate_dirty_incarnation(&first).is_ok();

    let merged = {
        let page_locked = page.write();
        cache
            .mark_page_dirty_page_locked_with_transition(0, &page_locked)?
            .ok_or(SystemError::EIO)?
    };
    let merged_ok = merged.kind() == PageCacheDirtyTransitionKind::MergedIntoDirty
        && merged.page_index() == 0
        && merged.dirty_incarnation() == first.dirty_incarnation();

    let entry = cache.inner.lock().get_entry(0).ok_or(SystemError::EIO)?;
    {
        let page_locked = page.write();
        if !cache.try_mark_page_writeback(0, page_locked.phys_address()) {
            return Ok(false);
        }
    }
    page.write().remove_flags(PageFlags::PG_DIRTY);

    // Hold the writer's page lock while an old completion is queued.  The
    // completion must remain blocked until the writer has both set PG_DIRTY
    // and published g2 under `inner`; otherwise it could merge this write
    // into g1 in the historic flag-to-cache gap.
    let race = Arc::new(PageCacheDirtyIncarnationRaceState::default());
    let race_cache = cache.clone();
    let race_entry = entry.clone();
    let race_page = page.clone();
    let race_state = race.clone();
    let redirtied = {
        let mut page_locked = page.write();
        page_locked.add_flags(PageFlags::PG_DIRTY);
        PAGECACHE_IO_WQS[0].enqueue(Work::new(move || {
            race_state.completion_started.store(true, Ordering::Release);
            race_state.wait.wake_all();
            let result = PageCacheManager::finish_writeback_entry_state(
                race_cache.clone(),
                0,
                race_entry.clone(),
                race_page.clone(),
                Ok(()),
                false,
            );
            race_state
                .completion_succeeded
                .store(result.is_ok(), Ordering::Release);
            race_state.completion_done.store(true, Ordering::Release);
            race_state.wait.wake_all();
        }));
        let completion_started = race
            .wait
            .wait_until_timeout(
                || {
                    race.completion_started
                        .load(Ordering::Acquire)
                        .then_some(())
                },
                Duration::from_secs(2),
            )
            .is_ok();
        let completion_blocked = completion_started
            && race
                .wait
                .wait_until_timeout(
                    || race.completion_done.load(Ordering::Acquire).then_some(()),
                    Duration::from_millis(25),
                )
                .is_err();
        let transition = match cache
            .mark_page_dirty_page_locked_with_transition(0, &page_locked)?
            .ok_or(SystemError::EIO)?
        {
            PageCacheDirtyTransition::Started(incarnation) => incarnation,
            PageCacheDirtyTransition::Merged(_) => return Ok(false),
        };
        (transition, completion_started && completion_blocked)
    };
    let (redirtied, completion_blocked_by_writer) = redirtied;
    let completion_finished = race
        .wait
        .wait_until_timeout(
            || race.completion_done.load(Ordering::Acquire).then_some(()),
            Duration::from_secs(2),
        )
        .is_ok();
    let redirty_ok = redirtied.kind() == PageCacheDirtyTransitionKind::RedirtiedDuringWriteback
        && redirtied.dirty_incarnation() == first.dirty_incarnation() + 1
        && cache.validate_dirty_incarnation(&first) == Err(SystemError::ESTALE)
        && cache.validate_dirty_incarnation(&redirtied).is_ok()
        && completion_blocked_by_writer
        && completion_finished
        && race.completion_succeeded.load(Ordering::Acquire)
        && {
            let inner = cache.inner.lock();
            entry.state() == PageState::Dirty
                && inner.dirty_pages.contains(&0)
                && !inner.writeback_pages.contains(&0)
        };
    if !first_ok || !merged_ok || !redirty_ok {
        return Ok(false);
    }

    let merged_successor = {
        let page_locked = page.write();
        cache
            .mark_page_dirty_page_locked_with_transition(0, &page_locked)?
            .ok_or(SystemError::EIO)?
    };
    if merged_successor.kind() != PageCacheDirtyTransitionKind::MergedIntoDirty
        || merged_successor.dirty_incarnation() != redirtied.dirty_incarnation()
    {
        return Ok(false);
    }

    // A clean completion retires g2.  The next front-end write must be g3,
    // not an ABA reuse of either completed generation.
    {
        let mut page_locked = page.write();
        if !cache.try_mark_page_writeback(0, page_locked.phys_address()) {
            return Ok(false);
        }
        page_locked.remove_flags(PageFlags::PG_DIRTY);
    }
    PageCacheManager::finish_writeback_entry_state(
        cache.clone(),
        0,
        entry,
        page.clone(),
        Ok(()),
        false,
    )?;
    let successor = {
        let mut page_locked = page.write();
        page_locked.add_flags(PageFlags::PG_DIRTY);
        match cache
            .mark_page_dirty_page_locked_with_transition(0, &page_locked)?
            .ok_or(SystemError::EIO)?
        {
            PageCacheDirtyTransition::Started(incarnation) => incarnation,
            PageCacheDirtyTransition::Merged(_) => return Ok(false),
        }
    };
    let successor_ok = successor.kind() == PageCacheDirtyTransitionKind::NewlyDirty
        && successor.dirty_incarnation() == redirtied.dirty_incarnation() + 1
        && cache.validate_dirty_incarnation(&successor).is_ok();

    let removed = cache.manager.remove_page(0)?.ok_or(SystemError::EIO)?;
    let paddr = removed.phys_address();
    page_manager_lock().remove_page(&paddr);
    let _ = page_reclaimer_lock().remove_page(&paddr);
    let primary_ok =
        successor_ok && cache.validate_dirty_incarnation(&successor) == Err(SystemError::ESTALE);
    Ok(primary_ok
        && run_dirty_incarnation_snapshot_successor_selftest()?
        && run_dirty_incarnation_entrypoint_selftest()?)
}

/// Exercise the other half of the page-lock protocol: an old batch snapshot
/// must not clear PG_DIRTY after a front writer has published its successor
/// incarnation.  This is deliberately sequenced as the former race window
/// (`claim g1`, `publish g2`, then `snapshot g1`) rather than probabilistically
/// relying on scheduler timing.
fn run_dirty_incarnation_snapshot_successor_selftest() -> Result<bool, SystemError> {
    let cache = PageCache::new_shmem(None, None);
    let page = cache.get_or_create_page_zero(0)?;
    let first = {
        let mut page_locked = page.write();
        page_locked.add_flags(PageFlags::PG_DIRTY);
        match cache
            .mark_page_dirty_page_locked_with_transition(0, &page_locked)?
            .ok_or(SystemError::EIO)?
        {
            PageCacheDirtyTransition::Started(incarnation) => incarnation,
            PageCacheDirtyTransition::Merged(_) => return Ok(false),
        }
    };
    let mut batch = match PageCacheManager::claim_next_writeback_batch(
        &cache,
        0,
        0,
        MMArch::PAGE_SIZE,
        None,
        false,
    )? {
        WritebackClaimOutcome::Claimed(batch) => batch,
        _ => return Ok(false),
    };
    let successor = {
        let mut page_locked = page.write();
        page_locked.add_flags(PageFlags::PG_DIRTY);
        match cache
            .mark_page_dirty_page_locked_with_transition(0, &page_locked)?
            .ok_or(SystemError::EIO)?
        {
            PageCacheDirtyTransition::Started(incarnation) => incarnation,
            PageCacheDirtyTransition::Merged(_) => return Ok(false),
        }
    };
    PageCacheManager::snapshot_writeback_batch(&mut batch)?;
    let snapshot_preserved_successor = page.read().flags().contains(PageFlags::PG_DIRTY)
        && cache.validate_dirty_incarnation(&first) == Err(SystemError::ESTALE)
        && cache.validate_dirty_incarnation(&successor).is_ok()
        && {
            let inner = cache.inner.lock();
            inner.dirty_pages.contains(&0) && inner.writeback_pages.contains(&0)
        };
    PageCacheManager::complete_writeback_batch(batch, Ok(()))?;
    let completion_preserved_successor = page.read().flags().contains(PageFlags::PG_DIRTY)
        && cache.validate_dirty_incarnation(&successor).is_ok()
        && {
            let entry = cache.inner.lock().get_entry(0).ok_or(SystemError::EIO)?;
            let inner = cache.inner.lock();
            entry.state() == PageState::Dirty
                && inner.dirty_pages.contains(&0)
                && !inner.writeback_pages.contains(&0)
        };
    let removed = cache.manager.remove_page(0)?.is_some();
    let paddr = page.phys_address();
    page_manager_lock().remove_page(&paddr);
    let _ = page_reclaimer_lock().remove_page(&paddr);
    Ok(snapshot_preserved_successor && completion_preserved_successor && removed)
}

/// Cover the non-buffered front publishers which intentionally retain the
/// capability only at their explicit future-bridge entry points.  The normal
/// wrappers stay lightweight, but these checks ensure prepared writes,
/// mmap-mkwrite, and FUSE-style ready-page merges obey the same lifetime.
fn run_dirty_incarnation_entrypoint_selftest() -> Result<bool, SystemError> {
    let prepared_cache = PageCache::new_shmem(None, None);
    let prepared_page = prepared_cache.get_or_create_page_zero(0)?;
    let prepared_transition = {
        let mut reservation = prepared_cache.prepare_page_dirty()?;
        let mut page_locked = prepared_page.write();
        page_locked.add_flags(PageFlags::PG_DIRTY);
        match prepared_cache
            .mark_page_dirty_prepared_page_locked_with_transition(
                0,
                &mut reservation,
                &page_locked,
            )?
            .ok_or(SystemError::EIO)?
        {
            PageCacheDirtyTransition::Started(incarnation) => incarnation,
            PageCacheDirtyTransition::Merged(_) => return Ok(false),
        }
    };
    let prepared_ok = prepared_transition.kind() == PageCacheDirtyTransitionKind::NewlyDirty
        && prepared_cache
            .validate_dirty_incarnation(&prepared_transition)
            .is_ok();
    let prepared_removed = prepared_cache.manager.remove_page(0)?.is_some();
    let prepared_paddr = prepared_page.phys_address();
    page_manager_lock().remove_page(&prepared_paddr);
    let _ = page_reclaimer_lock().remove_page(&prepared_paddr);

    let mkwrite_cache = PageCache::new_shmem(None, None);
    let mkwrite_page = mkwrite_cache.get_or_create_page_zero(0)?;
    let mkwrite_transition = match mkwrite_cache
        .manager()
        .prepare_page_mkwrite_with_transition(0, &mkwrite_page)?
    {
        PageCacheDirtyTransition::Started(incarnation) => incarnation,
        PageCacheDirtyTransition::Merged(_) => return Ok(false),
    };
    let mkwrite_entry = mkwrite_cache
        .inner
        .lock()
        .get_entry(0)
        .ok_or(SystemError::EIO)?;
    let mkwrite_generation = mkwrite_transition.dirty_incarnation();
    let fused_merge = mkwrite_cache.manager().update_ready_page(0, 0, &[0x5a])?
        && mkwrite_entry.dirty_incarnation() == mkwrite_generation
        && mkwrite_cache
            .validate_dirty_incarnation(&mkwrite_transition)
            .is_ok();
    let mkwrite_removed = mkwrite_cache.manager.remove_page(0)?.is_some();
    let mkwrite_paddr = mkwrite_page.phys_address();
    page_manager_lock().remove_page(&mkwrite_paddr);
    let _ = page_reclaimer_lock().remove_page(&mkwrite_paddr);

    // The future delayed-allocation front end needs its per-page capability
    // before PageCache releases the page lock to a concurrent claim. Its
    // external reservation is acquired before entering PageCache and would be
    // rolled back by its own Drop if this local operation failed.
    let handoff_cache = PageCache::new_shmem(None, None);
    let handoff_page = handoff_cache.get_or_create_page_zero(0)?;
    let handoff_data = [0xa5; MMArch::PAGE_SIZE];
    let mut captured_transition = None;
    let handoff_written =
        handoff_cache.write_single_full_page_with_transition(0, &handoff_data, |transition| {
            captured_transition = Some(transition);
        })?;
    let handoff_transition = captured_transition.take().ok_or(SystemError::EIO)?;
    let handoff_incarnation = handoff_transition.dirty_incarnation();
    let handoff_certificate = handoff_transition.certificate().ok_or(SystemError::EIO)?;
    let mut merged_transition = None;
    let handoff_merged_written =
        handoff_cache.write_single_full_page_with_transition(0, &handoff_data, |transition| {
            merged_transition = Some(transition);
        })?;
    let handoff_merged = merged_transition.take().ok_or(SystemError::EIO)?;
    let handoff_ok = handoff_written == MMArch::PAGE_SIZE
        && handoff_merged_written == MMArch::PAGE_SIZE
        && handoff_transition.kind() == PageCacheDirtyTransitionKind::NewlyDirty
        && handoff_certificate.page_index() == 0
        && handoff_certificate.cache_instance_id() != 0
        && handoff_certificate.entry_instance_id() != 0
        && handoff_certificate.dirty_incarnation() == handoff_incarnation
        && handoff_certificate.kind() == PageCacheDirtyTransitionKind::NewlyDirty
        && handoff_merged.kind() == PageCacheDirtyTransitionKind::MergedIntoDirty
        && handoff_merged.page_index() == 0
        && handoff_merged.dirty_incarnation() == handoff_incarnation
        && handoff_page.read().flags().contains(PageFlags::PG_DIRTY)
        && handoff_cache
            .validate_dirty_incarnation(match &handoff_transition {
                PageCacheDirtyTransition::Started(incarnation) => incarnation,
                PageCacheDirtyTransition::Merged(_) => return Ok(false),
            })
            .is_ok();
    let handoff_removed = handoff_cache.manager.remove_page(0)?.is_some();
    let handoff_paddr = handoff_page.phys_address();
    page_manager_lock().remove_page(&handoff_paddr);
    let _ = page_reclaimer_lock().remove_page(&handoff_paddr);

    // A front-write copy keeps a PageEntryPin until it has either published
    // its dirty incarnation or failed before publication.  A generic manager
    // removal must respect that pin, otherwise a successful external
    // reservation could observe ESTALE before the front handoff completes.
    let pin_cache = PageCache::new_shmem(None, None);
    let pin_page = pin_cache.get_or_create_page_zero(0)?;
    let pin = pin_cache
        .manager
        .peek_page_pinned(0)
        .ok_or(SystemError::EIO)?;
    let removal_blocked_by_pin = pin_cache.manager.remove_page(0)?.is_none();
    drop(pin);
    let removal_after_pin_release = pin_cache.manager.remove_page(0)?.is_some();
    let pin_paddr = pin_page.phys_address();
    page_manager_lock().remove_page(&pin_paddr);
    let _ = page_reclaimer_lock().remove_page(&pin_paddr);

    let rejected_cache = PageCache::new_shmem(None, None);
    let rejected_page = rejected_cache.get_or_create_page_zero(0)?;
    let rejected_data = [0x3c; MMArch::PAGE_SIZE];
    let rejected = rejected_cache.write_single_full_page_with_transition(1, &rejected_data, |_| {
        unreachable!("invalid front write must not publish a transition")
    });
    let rejected_clean = rejected == Err(SystemError::EINVAL)
        && !rejected_page.read().flags().contains(PageFlags::PG_DIRTY)
        && {
            let page_guard = rejected_page.read();
            unsafe { page_guard.as_slice()[0] == 0 }
        }
        && {
            let inner = rejected_cache.inner.lock();
            let entry = inner.get_entry(0).ok_or(SystemError::EIO)?;
            entry.state() == PageState::UpToDate
                && !inner.dirty_pages.contains(&0)
                && !inner.writeback_pages.contains(&0)
        };
    let rejected_removed = rejected_cache.manager.remove_page(0)?.is_some();
    let rejected_paddr = rejected_page.phys_address();
    page_manager_lock().remove_page(&rejected_paddr);
    let _ = page_reclaimer_lock().remove_page(&rejected_paddr);

    Ok(prepared_ok
        && prepared_removed
        && fused_merge
        && mkwrite_removed
        && handoff_ok
        && handoff_removed
        && removal_blocked_by_pin
        && removal_after_pin_release
        && rejected_clean
        && rejected_removed)
}

/// Exercise the descriptor half of the front-dirty certificate independently
/// from the future ext4 consumer.  In particular, g1 must remain frozen in
/// its already-bound descriptor while a writer creates g2 during g1's
/// Writeback state; only the next claim may observe g2.
fn run_single_page_token_certificate_selftest() -> Result<bool, SystemError> {
    let state = Arc::new(PageCacheSubmissionSelftestState::default());
    let backend = Arc::new(PageCacheSubmissionSelftestBackend {
        state: state.clone(),
        admission_order: PageCacheWritebackAdmissionOrder::AdmissionBeforeInvalidate,
        snapshot_phase: PageCacheWritebackSnapshotPhase::WithinAdmission,
    });
    let backend_dyn: Arc<dyn PageCacheBackend> = backend;
    // The selftest backend has no inode. Using shmem keeps its dirty publish
    // path honest without manufacturing a file-mapping retention owner.
    let cache = PageCache::new_shmem(None, Some(backend_dyn.clone()));
    let page = cache.get_or_create_page_zero(0)?;
    let mark_dirty = || -> Result<(), SystemError> {
        let mut page_locked = page.write();
        page_locked.add_flags(PageFlags::PG_DIRTY);
        cache.mark_page_dirty_page_locked(0, &page_locked)
    };
    let claimed_or_none = |outcome: WritebackClaimOutcome| match outcome {
        WritebackClaimOutcome::Claimed(batch) => Some(batch),
        WritebackClaimOutcome::NoBatch
        | WritebackClaimOutcome::Deferred(_)
        | WritebackClaimOutcome::FailedRecorded(_) => None,
    };

    mark_dirty()?;
    let mut first_batch = None;
    let first_admission =
        PageCacheManager::with_writeback_admission(&cache, &backend_dyn, &mut || {
            first_batch = claimed_or_none(PageCacheManager::claim_and_snapshot_locked(
                &cache,
                0,
                0,
                MMArch::PAGE_SIZE,
                true,
            )?);
            Ok(())
        })
        .is_ok();
    let Some(first_batch) = first_batch else {
        return Ok(false);
    };
    let first_certificate = first_batch
        .descriptor
        .dirty_certificate()
        .ok_or(SystemError::EIO)?;

    // This exact page is still Writeback for g1. The production dirty
    // publisher must create a distinct RedirtiedDuringWriteback generation,
    // not mutate the descriptor that already owns g1.
    mark_dirty()?;
    let first_descriptor_remains_frozen = first_batch.descriptor.dirty_certificate()
        == Some(first_certificate)
        && first_certificate.kind() == PageCacheDirtyTransitionKind::NewlyDirty;
    let first_submitted = matches!(
        PageCacheManager::submit_writeback_batch(first_batch)?,
        WritebackSubmitOutcome::Completed
    );
    let g2_visible_after_g1_completion = {
        let inner = cache.inner.lock();
        inner
            .get_entry(0)
            .is_some_and(|entry| entry.state() == PageState::Dirty)
            && inner.dirty_pages.contains(&0)
            && !inner.writeback_pages.contains(&0)
    };

    let mut second_batch = None;
    let second_admission =
        PageCacheManager::with_writeback_admission(&cache, &backend_dyn, &mut || {
            second_batch = claimed_or_none(PageCacheManager::claim_and_snapshot_locked(
                &cache,
                0,
                0,
                MMArch::PAGE_SIZE,
                true,
            )?);
            Ok(())
        })
        .is_ok();
    let Some(second_batch) = second_batch else {
        return Ok(false);
    };
    let second_certificate = second_batch
        .descriptor
        .dirty_certificate()
        .ok_or(SystemError::EIO)?;
    let second_submitted = matches!(
        PageCacheManager::submit_writeback_batch(second_batch)?,
        WritebackSubmitOutcome::Completed
    );

    let certificate_progression = first_certificate.cache_instance_id()
        == second_certificate.cache_instance_id()
        && first_certificate.entry_instance_id() == second_certificate.entry_instance_id()
        && first_certificate.page_index() == 0
        && second_certificate.page_index() == 0
        && first_certificate
            .dirty_incarnation()
            .checked_add(1)
            .is_some_and(|next| next == second_certificate.dirty_incarnation())
        && second_certificate.kind() == PageCacheDirtyTransitionKind::RedirtiedDuringWriteback;
    let removed = cache.manager.remove_page(0)?.is_some();
    let paddr = page.phys_address();
    page_manager_lock().remove_page(&paddr);
    let _ = page_reclaimer_lock().remove_page(&paddr);

    Ok(first_admission
        && first_descriptor_remains_frozen
        && first_submitted
        && g2_visible_after_g1_completion
        && second_admission
        && certificate_progression
        && second_submitted
        && removed
        && state.private_claims.load(Ordering::Acquire) == 0
        && state.single_page_certificates.load(Ordering::Acquire) == 2
        && state.certificate_errors.load(Ordering::Acquire) == 0)
}

pub(crate) fn run_accounting_debug_selftest() -> Result<alloc::string::String, SystemError> {
    if PAGECACHE_ACCOUNTING_SELFTEST_RUNNING
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return Err(SystemError::EBUSY);
    }
    let _running = PageCacheAccountingSelftestGuard;

    if !run_preallocated_batch_lifecycle_selftest()? {
        return Ok("status=fail stage=preallocated_batch_lifecycle\n".into());
    }

    let fault_invalidate_retry_order = run_invalidate_retry_lock_order_selftest().unwrap_or(false);
    if !fault_invalidate_retry_order {
        return Ok("status=fail stage=fault_invalidate_retry_order\n".into());
    }

    let tag_scan_chunk_release = run_tag_scan_chunk_release_selftest().unwrap_or(false);
    if !tag_scan_chunk_release {
        return Ok("status=fail stage=tag_scan_chunk_release\n".into());
    }

    let dirty_incarnation = match run_dirty_incarnation_selftest() {
        Ok(result) => result,
        Err(error) => {
            return Ok(alloc::format!(
                "status=fail stage=dirty_incarnation error={error:?}\n"
            ));
        }
    };
    if !dirty_incarnation {
        return Ok("status=fail stage=dirty_incarnation\n".into());
    }

    let single_page_token_certificate = match run_single_page_token_certificate_selftest() {
        Ok(result) => result,
        Err(error) => {
            return Ok(alloc::format!(
                "status=fail stage=single_page_token_certificate error={error:?}\n"
            ));
        }
    };
    if !single_page_token_certificate {
        return Ok("status=fail stage=single_page_token_certificate\n".into());
    }

    #[allow(dead_code)]
    struct PageEntryLayoutBaseline {
        page: Arc<Page>,
        instance_id: u64,
        state: AtomicU8,
        dirty_incarnation: AtomicU64,
        dirty_transition_kind: AtomicU8,
        writeback_tag: AtomicU64,
        writeback_incarnation: AtomicU64,
        accounted_unevictable: AtomicBool,
        active_users: AtomicUsize,
        wait_queue: WaitQueue,
    }

    let entry_size = core::mem::size_of::<PageEntry>();
    let baseline_size = core::mem::size_of::<PageEntryLayoutBaseline>();
    if entry_size > baseline_size {
        return Ok(alloc::format!(
            "status=fail stage=layout baseline_size={baseline_size} entry_size={entry_size}\n"
        ));
    }

    let writeback_budget_retry = run_async_writeback_budget_retry_selftest();
    if !writeback_budget_retry {
        return Ok("status=fail stage=writeback_budget_retry\n".into());
    }

    // All pre-existing fixture paths expect an immediate claim.  Keep their
    // setup concise while treating an unexpected defer as a failed claim; the
    // dedicated token fixture below exercises both defer outcomes explicitly.
    let claimed_or_none = |outcome: WritebackClaimOutcome| match outcome {
        WritebackClaimOutcome::Claimed(batch) => Some(batch),
        WritebackClaimOutcome::NoBatch
        | WritebackClaimOutcome::Deferred(_)
        | WritebackClaimOutcome::FailedRecorded(_) => None,
    };

    // The future ext4 delalloc backend opts into invalidate-before-admission so
    // its admission may acquire `size_lock -> io_lock`.  Exercise the actual
    // Dirty -> Writeback -> completion transition under that order, then prove
    // a concurrent invalidator prevents the try-only path from entering the
    // backend at all.
    let admission_backend = Arc::new(PageCacheAdmissionOrderSelftestBackend::new(
        PageCacheWritebackAdmissionOrder::InvalidateBeforeAdmission,
        true,
    ));
    let admission_backend_dyn: Arc<dyn PageCacheBackend> = admission_backend.clone();
    let admission_cache = PageCache::new(None, Some(admission_backend_dyn.clone()));
    *admission_backend.cache.lock() = Arc::downgrade(&admission_cache);
    let admission_page = admission_cache.get_or_create_page_zero(0)?;
    let mark_admission_dirty = || -> Result<(), SystemError> {
        admission_page.write().add_flags(PageFlags::PG_DIRTY);
        let mut inner = admission_cache.inner.lock();
        let entry = inner.get_entry(0).ok_or(SystemError::EIO)?;
        let old_state = entry.state();
        if old_state == PageState::Writeback {
            return Err(SystemError::EBUSY);
        }
        inner.dirty_pages.insert(0);
        entry.account_state_transition(old_state, PageState::Dirty);
        entry.set_state(PageState::Dirty);
        Ok(())
    };

    mark_admission_dirty()?;
    let mut blocking_batch = None;
    let blocking_admission = PageCacheManager::with_writeback_admission(
        &admission_cache,
        &admission_backend_dyn,
        &mut || {
            blocking_batch = claimed_or_none(PageCacheManager::claim_and_snapshot_locked(
                &admission_cache,
                0,
                0,
                MMArch::PAGE_SIZE,
                true,
            )?);
            Ok(())
        },
    )
    .is_ok();
    let Some(blocking_batch) = blocking_batch else {
        return Ok("status=fail stage=writeback_admission_blocking_claim\n".into());
    };
    PageCacheManager::complete_writeback_batch(blocking_batch, Ok(()))?;

    mark_admission_dirty()?;
    let mut try_batch = None;
    let try_admission = PageCacheManager::try_with_writeback_admission(
        &admission_cache,
        &admission_backend_dyn,
        &mut || {
            try_batch = claimed_or_none(PageCacheManager::claim_and_snapshot_locked(
                &admission_cache,
                0,
                0,
                MMArch::PAGE_SIZE,
                true,
            )?);
            Ok(())
        },
    )
    .unwrap_or(false);
    let Some(try_batch) = try_batch else {
        return Ok("status=fail stage=writeback_admission_try_claim\n".into());
    };
    PageCacheManager::complete_writeback_batch(try_batch, Ok(()))?;

    mark_admission_dirty()?;
    let callbacks_before_invalidate = admission_backend
        .observed_expected_order
        .load(Ordering::Acquire);
    let mut claim_entered_while_invalidating = false;
    let try_rejected_by_invalidation = {
        let _invalidate = admission_cache.invalidate_write();
        !PageCacheManager::try_with_writeback_admission(
            &admission_cache,
            &admission_backend_dyn,
            &mut || {
                claim_entered_while_invalidating = true;
                Ok(())
            },
        )
        .unwrap_or(true)
    };
    let try_rejected_page_state_ok = {
        let inner = admission_cache.inner.lock();
        inner
            .get_entry(0)
            .is_some_and(|entry| entry.state() == PageState::Dirty)
            && inner.dirty_pages.contains(&0)
            && !inner.writeback_pages.contains(&0)
    };
    // A try-only backend may obtain its admission locks yet find that a
    // stable EOF is unavailable without waiting.  That is a normal skip, not
    // a writeback error: PageCache must not publish the dirty page as
    // Writeback or clear its dirty membership.
    let errseq_before_try_stable_size_skip = admission_cache.sample_writeback_error();
    let try_stable_size_skip = matches!(
        PageCacheManager::try_claim_and_snapshot_within_admission_with_stable_size(
            &admission_cache,
            &admission_backend_dyn,
            0,
            0,
            || Ok(None),
        )?,
        WritebackClaimOutcome::NoBatch
    );
    let try_stable_size_skip_page_state_ok = {
        let inner = admission_cache.inner.lock();
        inner
            .get_entry(0)
            .is_some_and(|entry| entry.state() == PageState::Dirty)
            && inner.dirty_pages.contains(&0)
            && !inner.writeback_pages.contains(&0)
    };
    let try_stable_size_skip_errseq_clean = admission_cache
        .check_writeback_error_since(errseq_before_try_stable_size_skip)
        .is_none();
    let admission_order_ok = blocking_admission
        && try_admission
        && try_rejected_by_invalidation
        && !claim_entered_while_invalidating
        && try_rejected_page_state_ok
        && try_stable_size_skip
        && try_stable_size_skip_page_state_ok
        && try_stable_size_skip_errseq_clean
        && admission_backend
            .observed_expected_order
            .load(Ordering::Acquire)
            == 3
        && callbacks_before_invalidate == 2
        && admission_backend
            .observed_unexpected_order
            .load(Ordering::Acquire)
            == 0;
    if !admission_order_ok {
        return Ok(alloc::format!(
            "status=fail stage=writeback_admission_order blocking={blocking_admission} try={try_admission} rejected={try_rejected_by_invalidation} entered={claim_entered_while_invalidating} dirty={try_rejected_page_state_ok} stable_skip={try_stable_size_skip} stable_skip_dirty={try_stable_size_skip_page_state_ok} stable_skip_errseq_clean={try_stable_size_skip_errseq_clean} expected={} unexpected={}\n",
            admission_backend.observed_expected_order.load(Ordering::Acquire),
            admission_backend.observed_unexpected_order.load(Ordering::Acquire),
        ));
    }
    let admission_removed = admission_cache.manager.remove_page(0)?.is_some();
    let admission_paddr = admission_page.phys_address();
    page_manager_lock().remove_page(&admission_paddr);
    let _ = page_reclaimer_lock().remove_page(&admission_paddr);
    if !admission_removed {
        return Ok("status=fail stage=writeback_admission_cleanup\n".into());
    }
    drop(admission_cache);

    // Generic/FUSE backends keep the established admission-before-invalidate
    // order.  This direct contract check prevents a future default flip from
    // silently recreating the FUSE barrier/invalidation ABBA cycle.
    let legacy_backend = Arc::new(PageCacheAdmissionOrderSelftestBackend::new(
        PageCacheWritebackAdmissionOrder::AdmissionBeforeInvalidate,
        false,
    ));
    let legacy_backend_dyn: Arc<dyn PageCacheBackend> = legacy_backend.clone();
    let legacy_cache = PageCache::new(None, Some(legacy_backend_dyn.clone()));
    *legacy_backend.cache.lock() = Arc::downgrade(&legacy_cache);
    let mut legacy_blocking_claim = || Ok(());
    let legacy_blocking = PageCacheManager::with_writeback_admission(
        &legacy_cache,
        &legacy_backend_dyn,
        &mut legacy_blocking_claim,
    )
    .is_ok();
    let mut legacy_try_claim = || Ok(());
    let legacy_try = PageCacheManager::try_with_writeback_admission(
        &legacy_cache,
        &legacy_backend_dyn,
        &mut legacy_try_claim,
    )
    .unwrap_or(false);
    // A Legacy backend still receives a normal descriptor and still follows
    // the original submit path, but it must not consume a Token generation.
    // Keep this explicit: otherwise a future refactor can reintroduce a
    // counter mutation on the eager ext4/FUSE hot path without breaking token
    // lifecycle tests.
    let legacy_page = legacy_cache.get_or_create_page_zero(0)?;
    let legacy_entry = {
        let inner = legacy_cache.inner.lock();
        inner.get_entry(0).ok_or(SystemError::EIO)?
    };
    legacy_page.write().add_flags(PageFlags::PG_DIRTY);
    {
        let mut inner = legacy_cache.inner.lock();
        let current = inner.get_entry(0).ok_or(SystemError::EIO)?;
        if !Arc::ptr_eq(&current, &legacy_entry) {
            return Err(SystemError::EIO);
        }
        let old_state = legacy_entry.state();
        inner.dirty_pages.insert(0);
        legacy_entry.account_state_transition(old_state, PageState::Dirty);
        legacy_entry.set_state(PageState::Dirty);
    }
    let legacy_generation_before = legacy_cache.inner.lock().next_writeback_generation;
    let mut legacy_batch = None;
    let legacy_submission_admission =
        PageCacheManager::with_writeback_admission(&legacy_cache, &legacy_backend_dyn, &mut || {
            legacy_batch = claimed_or_none(PageCacheManager::claim_and_snapshot_locked(
                &legacy_cache,
                0,
                0,
                MMArch::PAGE_SIZE,
                true,
            )?);
            Ok(())
        })
        .is_ok();
    let legacy_generation_is_zero = match legacy_batch {
        Some(batch) => {
            let generation_is_zero = batch.descriptor.writeback_generation() == 0;
            generation_is_zero && PageCacheManager::submit_writeback_batch(batch).is_ok()
        }
        None => false,
    };
    let legacy_generation_unchanged =
        legacy_cache.inner.lock().next_writeback_generation == legacy_generation_before;
    let legacy_removed = legacy_cache.manager.remove_page(0)?.is_some();
    // A frozen waiter which observed incarnation 1 must not attach itself to
    // incarnation 2 of the same Arc<PageEntry> after the old completion.
    legacy_entry
        .writeback_incarnation
        .store(2, Ordering::Release);
    legacy_entry.set_state(PageState::Writeback);
    let frozen_writeback_aba_ok = matches!(
        PageCacheManager::writeback_incarnation_result(&legacy_entry, 1),
        Some(Ok(()))
    );
    legacy_entry.set_state(PageState::UpToDate);
    let legacy_paddr = legacy_page.phys_address();
    page_manager_lock().remove_page(&legacy_paddr);
    let _ = page_reclaimer_lock().remove_page(&legacy_paddr);
    if !legacy_blocking
        || !legacy_try
        || !legacy_submission_admission
        || !legacy_generation_is_zero
        || !legacy_generation_unchanged
        || !legacy_removed
        || !frozen_writeback_aba_ok
        || legacy_backend
            .observed_expected_order
            .load(Ordering::Acquire)
            != 3
        || legacy_backend
            .observed_unexpected_order
            .load(Ordering::Acquire)
            != 0
    {
        return Ok(alloc::format!(
            "status=fail stage=writeback_admission_legacy blocking={legacy_blocking} try={legacy_try} submission={legacy_submission_admission} generation_zero={legacy_generation_is_zero} generation_unchanged={legacy_generation_unchanged} removed={legacy_removed} frozen_aba={frozen_writeback_aba_ok} expected={} unexpected={}\n",
            legacy_backend.observed_expected_order.load(Ordering::Acquire),
            legacy_backend.observed_unexpected_order.load(Ordering::Acquire),
        ));
    }
    drop(legacy_cache);

    // A filesystem-specific token is bound before Dirty -> Writeback and is
    // then the only submission path.  Exercise a multi-page partial-EOF
    // submission, the real shared snapshot-failure cleanup branch (with the
    // failure injected into its snapshot action while admission is held), a
    // bind failure, a token submission failure, and a zero-payload snapshot.
    // The synthetic backend's legacy write function deliberately fails, so
    // every successful token path would expose a regression which silently
    // fell back to write_pages().
    let submission_state = Arc::new(PageCacheSubmissionSelftestState::default());
    let submission_backend = Arc::new(PageCacheSubmissionSelftestBackend {
        state: submission_state.clone(),
        admission_order: PageCacheWritebackAdmissionOrder::AdmissionBeforeInvalidate,
        snapshot_phase: PageCacheWritebackSnapshotPhase::WithinAdmission,
    });
    let submission_backend_dyn: Arc<dyn PageCacheBackend> = submission_backend;
    // This synthetic token backend has no inode owner.  Keep it shmem-backed
    // so the production front-dirty publisher exercises its state machine
    // without asking a deliberately absent file mapping for retention.
    let submission_cache = PageCache::new_shmem(None, Some(submission_backend_dyn.clone()));
    let submission_page0 = submission_cache.get_or_create_page_zero(0)?;
    let submission_page1 = submission_cache.get_or_create_page_zero(1)?;
    // A Token declaration is already sufficient to forbid the legacy
    // one-page helper.  This covers the interval before the first normal
    // bind has locked `writeback_protocol` as well as the steady state below.
    let token_direct_rejected_before_bind = {
        let page_locked = submission_page0.write();
        !submission_cache.try_mark_page_writeback(0, page_locked.phys_address())
    };
    let token_file_size = MMArch::PAGE_SIZE
        .checked_mul(2)
        .and_then(|size| size.checked_sub(37))
        .ok_or(SystemError::EOVERFLOW)?;
    let mark_submission_dirty = |page_index: usize| -> Result<(), SystemError> {
        let entry = submission_cache
            .inner
            .lock()
            .get_entry(page_index)
            .ok_or(SystemError::EIO)?;
        let mut page_locked = entry.page.write();
        page_locked.add_flags(PageFlags::PG_DIRTY);
        submission_cache.mark_page_dirty_page_locked(page_index, &page_locked)
    };
    let wait_for_deferred_progress =
        |progress: Arc<dyn PageCacheWritebackProgress>, expected_waiters: usize| {
            progress.arm();
            let completed = Arc::new(AtomicBool::new(false));
            let completed_wait = Arc::new(WaitQueue::default());
            let retry_completed = Arc::new(AtomicBool::new(false));
            let progress_slot = Arc::new(Mutex::new(Some(progress.clone())));
            let worker_completed = completed.clone();
            let worker_completed_wait = completed_wait.clone();
            let worker_progress_slot = progress_slot.clone();
            schedule_work(Work::new(move || {
                let Some(progress) = worker_progress_slot.lock().take() else {
                    return;
                };
                progress.wait_for_progress();
                worker_completed.store(true, Ordering::Release);
                worker_completed_wait.wake_all();
            }));
            let retry_completed_callback = retry_completed.clone();
            let retry_completed_wait = completed_wait.clone();
            progress.register_retry(Arc::new(move |_| {
                retry_completed_callback.store(true, Ordering::Release);
                retry_completed_wait.wake_all();
            }));
            submission_state.progress_wait.wait_until(|| {
                (submission_state
                    .deferred_waiters_entered
                    .load(Ordering::Acquire)
                    >= expected_waiters)
                    .then_some(())
            });
            let blocked_before_release = !completed.load(Ordering::Acquire);
            let retry_blocked_before_release = !retry_completed.load(Ordering::Acquire);
            submission_state.release_deferred_progress();
            completed_wait.wait_until(|| {
                (completed.load(Ordering::Acquire) && retry_completed.load(Ordering::Acquire))
                    .then_some(())
            });
            let late_retry_completed = Arc::new(AtomicBool::new(false));
            let late_retry_completed_callback = late_retry_completed.clone();
            progress.register_retry(Arc::new(move |_| {
                late_retry_completed_callback.store(true, Ordering::Release);
            }));
            let late_retry_runs_immediately = late_retry_completed.load(Ordering::Acquire);
            // The first half proves synchronous callers can block on the
            // producer predicate. The second proves async/reclaimer callers
            // receive a producer-driven continuation without consuming a
            // permanently blocked PageCache worker. The final check covers
            // the other side of the registration race: a continuation
            // registered after progress observes the advanced sequence and
            // must be invoked synchronously instead of being stranded.
            blocked_before_release && retry_blocked_before_release && late_retry_runs_immediately
        };

    mark_submission_dirty(0)?;
    mark_submission_dirty(1)?;
    let mut submitted_batch = None;
    let submitted_admission = PageCacheManager::with_writeback_admission(
        &submission_cache,
        &submission_backend_dyn,
        &mut || {
            submitted_batch = claimed_or_none(PageCacheManager::claim_and_snapshot_locked(
                &submission_cache,
                0,
                1,
                token_file_size,
                true,
            )?);
            Ok(())
        },
    )
    .is_ok();
    let submitted = match submitted_batch {
        Some(batch) => PageCacheManager::submit_writeback_batch(batch).is_ok(),
        None => false,
    };
    let token_direct_rejected_after_lock = {
        let page_locked = submission_page0.write();
        !submission_cache.try_mark_page_writeback(0, page_locked.phys_address())
    };
    let multi_page_descriptor_ok = submission_state.first_index.load(Ordering::Acquire) == 0
        && submission_state.last_index.load(Ordering::Acquire) == 1
        && submission_state.file_size.load(Ordering::Acquire) == token_file_size
        && submission_state.valid_bytes.load(Ordering::Acquire) == token_file_size
        && submission_state
            .last_writeback_generation
            .load(Ordering::Acquire)
            != 0;

    mark_submission_dirty(0)?;
    let mut snapshot_error_outcome = None;
    let snapshot_error_admission = PageCacheManager::with_writeback_admission(
        &submission_cache,
        &submission_backend_dyn,
        &mut || {
            snapshot_error_outcome = Some(PageCacheManager::claim_and_snapshot_locked_with(
                &submission_cache,
                0,
                0,
                token_file_size,
                None,
                true,
                |_batch| Err(SystemError::EIO),
            )?);
            Ok(())
        },
    )
    .is_ok();
    let snapshot_error = snapshot_error_admission
        && matches!(
            snapshot_error_outcome,
            Some(WritebackClaimOutcome::FailedRecorded(SystemError::EIO))
        );
    let snapshot_error_page_redirtied = {
        let inner = submission_cache.inner.lock();
        inner
            .get_entry(0)
            .is_some_and(|entry| entry.state() == PageState::Dirty)
            && inner.dirty_pages.contains(&0)
            && !inner.writeback_pages.contains(&0)
    };
    submission_state
        .fail_next_bind
        .store(true, Ordering::Release);
    let mut bind_error_batch = None;
    let bind_error = PageCacheManager::with_writeback_admission(
        &submission_cache,
        &submission_backend_dyn,
        &mut || {
            bind_error_batch = claimed_or_none(PageCacheManager::claim_and_snapshot_locked(
                &submission_cache,
                0,
                0,
                token_file_size,
                true,
            )?);
            Ok(())
        },
    )
    .err()
        == Some(SystemError::EIO);
    let bind_error_page_stayed_dirty = {
        let inner = submission_cache.inner.lock();
        inner
            .get_entry(0)
            .is_some_and(|entry| entry.state() == PageState::Dirty)
            && inner.dirty_pages.contains(&0)
            && !inner.writeback_pages.contains(&0)
    };
    let errseq_before_submit_error = submission_cache.sample_writeback_error();
    submission_state
        .fail_next_submit
        .store(true, Ordering::Release);
    let mut submit_error_batch = None;
    let submit_error_admission = PageCacheManager::with_writeback_admission(
        &submission_cache,
        &submission_backend_dyn,
        &mut || {
            submit_error_batch = claimed_or_none(PageCacheManager::claim_and_snapshot_locked(
                &submission_cache,
                0,
                0,
                token_file_size,
                true,
            )?);
            Ok(())
        },
    )
    .is_ok();
    let submit_error = match submit_error_batch {
        Some(batch) => matches!(
            PageCacheManager::submit_writeback_batch(batch),
            Ok(WritebackSubmitOutcome::Failed(SystemError::EIO))
        ),
        None => false,
    };
    let submit_error_page_redirtied = {
        let inner = submission_cache.inner.lock();
        inner
            .get_entry(0)
            .is_some_and(|entry| entry.state() == PageState::Dirty)
            && inner.dirty_pages.contains(&0)
            && !inner.writeback_pages.contains(&0)
    };
    let submit_error_recorded = submission_cache
        .check_writeback_error_since(errseq_before_submit_error)
        == Some(SystemError::EIO);

    // A mapper may discover only after snapshotting that it must wait for
    // ordered progress.  This is not a writeback error: its token resolves
    // the private claim, the batch returns to Dirty, and fsync-style callers
    // wait on the producer ticket before retrying.
    // `check_writeback_error_since()` intentionally does not advance an
    // errseq observer.  Consume the synthetic terminal failure before using
    // the same cache to prove that a later defer records no *new* error.
    let mut acknowledged_submit_error = errseq_before_submit_error;
    let submit_error_acknowledged = submission_cache
        .writeback_error
        .check_and_advance(&mut acknowledged_submit_error);
    let errseq_before_submit_defer = submission_cache.sample_writeback_error();
    // Use two pages here: deferred completion must republish the complete
    // descriptor segment atomically, never expose page 0 as Dirty while page
    // 1 is still Writeback and therefore permit a prefix reclaim/claim.
    mark_submission_dirty(1)?;
    submission_state
        .defer_next_submit
        .store(true, Ordering::Release);
    let mut submit_defer_batch = None;
    let submit_defer_admission = PageCacheManager::with_writeback_admission(
        &submission_cache,
        &submission_backend_dyn,
        &mut || {
            submit_defer_batch = claimed_or_none(PageCacheManager::claim_and_snapshot_locked(
                &submission_cache,
                0,
                1,
                token_file_size,
                true,
            )?);
            Ok(())
        },
    )
    .is_ok();
    let submit_defer_progress = match submit_defer_batch {
        Some(batch) => match PageCacheManager::submit_writeback_batch(batch) {
            Ok(WritebackSubmitOutcome::Deferred(progress)) => Some(progress),
            Ok(WritebackSubmitOutcome::Completed)
            | Ok(WritebackSubmitOutcome::Failed(_))
            | Err(_) => None,
        },
        None => None,
    };
    let submit_defer_page_redirtied = {
        let (entry0, entry1) = {
            let inner = submission_cache.inner.lock();
            (inner.get_entry(0), inner.get_entry(1))
        };
        let flags_dirty = entry0
            .as_ref()
            .is_some_and(|entry| entry.page.read().flags().contains(PageFlags::PG_DIRTY))
            && entry1
                .as_ref()
                .is_some_and(|entry| entry.page.read().flags().contains(PageFlags::PG_DIRTY));
        flags_dirty && {
            let inner = submission_cache.inner.lock();
            entry0.as_ref().is_some_and(|entry| {
                inner
                    .get_entry(0)
                    .is_some_and(|current| Arc::ptr_eq(&current, entry))
                    && entry.state() == PageState::Dirty
            }) && entry1.as_ref().is_some_and(|entry| {
                inner
                    .get_entry(1)
                    .is_some_and(|current| Arc::ptr_eq(&current, entry))
                    && entry.state() == PageState::Dirty
            }) && inner.dirty_pages.contains(&0)
                && inner.dirty_pages.contains(&1)
                && !inner.writeback_pages.contains(&0)
                && !inner.writeback_pages.contains(&1)
        }
    };
    let submit_defer_preserves_page_error = submission_cache
        .inner
        .lock()
        .get_entry(0)
        .is_some_and(|entry| entry.page.read().flags().contains(PageFlags::PG_ERROR));
    let errseq_after_submit_defer = submission_cache.sample_writeback_error();
    let submit_defer_no_errseq = errseq_after_submit_defer == errseq_before_submit_defer;
    let submit_defer_waited = if let Some(progress) = submit_defer_progress {
        wait_for_deferred_progress(progress, 1)
    } else {
        false
    };
    let mut submit_defer_retry_batch = None;
    let submit_defer_retry_admission = PageCacheManager::with_writeback_admission(
        &submission_cache,
        &submission_backend_dyn,
        &mut || {
            submit_defer_retry_batch =
                claimed_or_none(PageCacheManager::claim_and_snapshot_locked(
                    &submission_cache,
                    0,
                    1,
                    token_file_size,
                    true,
                )?);
            Ok(())
        },
    )
    .is_ok();
    let submit_defer_retry_completed = matches!(
        submit_defer_retry_batch
            .map(PageCacheManager::submit_writeback_batch)
            .transpose(),
        Ok(Some(WritebackSubmitOutcome::Completed))
    );
    let submit_defer_retry_clears_page_error = !submission_cache
        .inner
        .lock()
        .get_entry(0)
        .is_some_and(|entry| entry.page.read().flags().contains(PageFlags::PG_ERROR));

    // A queue follower can be rejected before Dirty -> Writeback publication.
    // Its ticket proves that the synchronous path has a blocking progress edge
    // rather than re-claiming the same Dirty page in a hot loop.
    mark_submission_dirty(0)?;
    submission_state
        .defer_next_bind
        .store(true, Ordering::Release);
    let mut claim_defer = WritebackClaimOutcome::NoBatch;
    let claim_defer_admission = PageCacheManager::with_writeback_admission(
        &submission_cache,
        &submission_backend_dyn,
        &mut || {
            claim_defer = PageCacheManager::claim_and_snapshot_locked(
                &submission_cache,
                0,
                0,
                token_file_size,
                true,
            )?;
            Ok(())
        },
    )
    .is_ok();
    let claim_defer_progress = match claim_defer {
        WritebackClaimOutcome::Deferred(progress) => Some(progress),
        WritebackClaimOutcome::NoBatch
        | WritebackClaimOutcome::Claimed(_)
        | WritebackClaimOutcome::FailedRecorded(_) => None,
    };
    let claim_defer_page_untouched = {
        let inner = submission_cache.inner.lock();
        inner
            .get_entry(0)
            .is_some_and(|entry| entry.state() == PageState::Dirty)
            && inner.dirty_pages.contains(&0)
            && !inner.writeback_pages.contains(&0)
    };
    let claim_defer_waited = if let Some(progress) = claim_defer_progress {
        wait_for_deferred_progress(progress, 2)
    } else {
        false
    };
    let mut claim_defer_retry_batch = None;
    let claim_defer_retry_admission = PageCacheManager::with_writeback_admission(
        &submission_cache,
        &submission_backend_dyn,
        &mut || {
            claim_defer_retry_batch = claimed_or_none(PageCacheManager::claim_and_snapshot_locked(
                &submission_cache,
                0,
                0,
                token_file_size,
                true,
            )?);
            Ok(())
        },
    )
    .is_ok();
    let claim_defer_retry_completed = matches!(
        claim_defer_retry_batch
            .map(PageCacheManager::submit_writeback_batch)
            .transpose(),
        Ok(Some(WritebackSubmitOutcome::Completed))
    );

    mark_submission_dirty(0)?;
    let (tagged_entry, tagged_epoch) = {
        let inner = submission_cache.inner.lock();
        let entry = inner.get_entry(0).ok_or(SystemError::EIO)?;
        let epoch = 0x5a5a_u64;
        entry.set_writeback_tag(epoch);
        (entry, epoch)
    };
    let errseq_before_tagged_snapshot_error = submission_cache.sample_writeback_error();
    let mut tagged_snapshot_error_outcome = None;
    let tagged_snapshot_error_admission = PageCacheManager::with_writeback_admission(
        &submission_cache,
        &submission_backend_dyn,
        &mut || {
            tagged_snapshot_error_outcome = Some(PageCacheManager::claim_and_snapshot_locked_with(
                &submission_cache,
                0,
                0,
                token_file_size,
                Some((0, &tagged_entry, tagged_epoch)),
                true,
                |_batch| Err(SystemError::EIO),
            )?);
            Ok(())
        },
    )
    .is_ok();
    let tagged_snapshot_error = tagged_snapshot_error_admission
        && matches!(
            tagged_snapshot_error_outcome,
            Some(WritebackClaimOutcome::FailedRecorded(SystemError::EIO))
        );
    // A tagged caller receives `FailedRecorded` after PageCache has already
    // made the error visible. It must only retire the frozen generation; a
    // second `record_writeback_error` would be observable as a duplicate
    // errseq event to a file descriptor that acknowledged the first one.
    let mut tagged_snapshot_error_ack = errseq_before_tagged_snapshot_error;
    let tagged_snapshot_error_recorded_once = submission_cache
        .writeback_error
        .check_and_advance(&mut tagged_snapshot_error_ack)
        == Some(SystemError::EIO);
    PageCacheManager::retire_tagged_writeback_generation(&submission_cache, 0, 0, tagged_epoch);
    let tagged_snapshot_error_not_recorded_twice = submission_cache
        .writeback_error
        .check_and_advance(&mut tagged_snapshot_error_ack)
        .is_none();
    let tagged_snapshot_error_page_redirtied = {
        let inner = submission_cache.inner.lock();
        inner
            .get_entry(0)
            .is_some_and(|entry| entry.state() == PageState::Dirty)
            && inner.dirty_pages.contains(&0)
            && !inner.writeback_pages.contains(&0)
            && tagged_entry.writeback_tag() == 0
    };

    let bind_attempts_before_zero_payload = submission_state.bind_attempts.load(Ordering::Acquire);
    let generation_before_zero_payload = submission_cache.inner.lock().next_writeback_generation;
    let mut zero_payload_batch = None;
    let zero_payload_admission = PageCacheManager::with_writeback_admission(
        &submission_cache,
        &submission_backend_dyn,
        &mut || {
            zero_payload_batch = claimed_or_none(PageCacheManager::claim_and_snapshot_locked(
                &submission_cache,
                0,
                0,
                0,
                true,
            )?);
            Ok(())
        },
    )
    .is_ok();
    let zero_payload_descriptor_is_zero = match zero_payload_batch {
        Some(batch) => {
            batch.descriptor.writeback_generation() == 0
                && PageCacheManager::submit_writeback_batch(batch).is_ok()
        }
        None => false,
    };
    let zero_payload_retired = zero_payload_descriptor_is_zero;
    let zero_payload_bypassed =
        submission_state.bind_attempts.load(Ordering::Acquire) == bind_attempts_before_zero_payload;
    let zero_payload_generation_unchanged =
        submission_cache.inner.lock().next_writeback_generation == generation_before_zero_payload;
    // Keep the production `page -> inner` order: taking a page lock while
    // holding `inner` could deadlock with dirty-page publication.  Revalidate
    // the entry identity after sampling the page flags so this remains an
    // exact post-completion assertion rather than a stale observation.
    let zero_payload_entry = {
        let inner = submission_cache.inner.lock();
        inner.get_entry(0).ok_or(SystemError::EIO)?
    };
    let zero_payload_page_clean = !zero_payload_entry
        .page
        .read()
        .flags()
        .intersects(PageFlags::PG_DIRTY | PageFlags::PG_WRITEBACK);
    let zero_payload_page_retired = zero_payload_page_clean && {
        let inner = submission_cache.inner.lock();
        inner.get_entry(0).is_some_and(|entry| {
            Arc::ptr_eq(&entry, &zero_payload_entry) && entry.state() == PageState::UpToDate
        }) && !inner.dirty_pages.contains(&0)
            && !inner.writeback_pages.contains(&0)
    };
    // Generation exhaustion is an admission failure, not an ABA wrap. Keep
    // this in an independent cache: the main synthetic cache continues with
    // tracker/cancellation scenarios below, so forcing its counter to MAX
    // would turn a coverage check into artificial cross-test interference.
    let overflow_state = Arc::new(PageCacheSubmissionSelftestState::default());
    let overflow_backend: Arc<dyn PageCacheBackend> =
        Arc::new(PageCacheSubmissionSelftestBackend {
            state: overflow_state.clone(),
            admission_order: PageCacheWritebackAdmissionOrder::AdmissionBeforeInvalidate,
            snapshot_phase: PageCacheWritebackSnapshotPhase::WithinAdmission,
        });
    let overflow_cache = PageCache::new(None, Some(overflow_backend.clone()));
    let overflow_page = overflow_cache.get_or_create_page_zero(0)?;
    let overflow_entry = {
        let inner = overflow_cache.inner.lock();
        inner.get_entry(0).ok_or(SystemError::EIO)?
    };
    overflow_page.write().add_flags(PageFlags::PG_DIRTY);
    {
        let mut inner = overflow_cache.inner.lock();
        let current = inner.get_entry(0).ok_or(SystemError::EIO)?;
        if !Arc::ptr_eq(&current, &overflow_entry) {
            return Err(SystemError::EIO);
        }
        let old_state = overflow_entry.state();
        inner.dirty_pages.insert(0);
        overflow_entry.account_state_transition(old_state, PageState::Dirty);
        overflow_entry.set_state(PageState::Dirty);
        inner.next_writeback_generation = u64::MAX;
    }
    let overflow_generation_rejected =
        PageCacheManager::with_writeback_admission(&overflow_cache, &overflow_backend, &mut || {
            let _ = PageCacheManager::claim_and_snapshot_locked(
                &overflow_cache,
                0,
                0,
                MMArch::PAGE_SIZE,
                true,
            )?;
            Ok(())
        })
        .err()
            == Some(SystemError::EOVERFLOW);
    // Preserve the production page -> inner lock order. Sample page flags
    // before taking `inner`, then revalidate the entry identity alongside its
    // logical Dirty/writeback memberships.
    let overflow_page_flags_preserved = {
        let page = overflow_entry.page.read();
        let flags = page.flags();
        flags.contains(PageFlags::PG_DIRTY) && !flags.contains(PageFlags::PG_WRITEBACK)
    };
    let overflow_generation_preserves_dirty = overflow_page_flags_preserved && {
        let inner = overflow_cache.inner.lock();
        inner.next_writeback_generation == u64::MAX
            && inner.get_entry(0).is_some_and(|entry| {
                Arc::ptr_eq(&entry, &overflow_entry) && entry.state() == PageState::Dirty
            })
            && inner.dirty_pages.contains(&0)
            && !inner.writeback_pages.contains(&0)
            && overflow_state.bind_attempts.load(Ordering::Acquire) == 0
    };
    let overflow_removed = overflow_cache.manager.remove_page(0)?.is_some();
    let overflow_paddr = overflow_page.phys_address();
    page_manager_lock().remove_page(&overflow_paddr);
    let _ = page_reclaimer_lock().remove_page(&overflow_paddr);
    let submission_defer_ok = submit_defer_admission
        && submit_defer_page_redirtied
        && submit_defer_preserves_page_error
        && submit_defer_no_errseq
        && submit_defer_waited
        && submit_defer_retry_admission
        && submit_defer_retry_completed
        && submit_defer_retry_clears_page_error
        && claim_defer_admission
        && claim_defer_page_untouched
        && claim_defer_waited
        && claim_defer_retry_admission
        && claim_defer_retry_completed
        && submission_state
            .deferred_before_claim
            .load(Ordering::Acquire)
            == 1
        && submission_state
            .deferred_after_submit
            .load(Ordering::Acquire)
            == 1
        && submission_state.deferred_waits.load(Ordering::Acquire) == 2;

    // A legacy/within-admission backend may report a terminal admission
    // error only after its callback has claimed and snapshotted a token. At
    // that point neither admission nor invalidate-read is retained, so this
    // must select the explicitly safe no-invalidate cancellation context
    // rather than the ext4 split finalizer.
    mark_submission_dirty(0)?;
    submission_state
        .fail_admission_after_claim
        .store(true, Ordering::Release);
    let within_admission_post_claim_error = matches!(
        PageCacheManager::try_claim_and_snapshot_within_admission_with_stable_size(
            &submission_cache,
            &submission_backend_dyn,
            0,
            0,
            || Ok(Some(token_file_size)),
        ),
        Ok(WritebackClaimOutcome::FailedRecorded(SystemError::EIO))
    );
    let within_admission_post_claim_error_redirtied = {
        let inner = submission_cache.inner.lock();
        inner
            .get_entry(0)
            .is_some_and(|entry| entry.state() == PageState::Dirty)
            && inner.dirty_pages.contains(&0)
            && !inner.writeback_pages.contains(&0)
    };

    // Ext4's future delayed mapper must release `size_lock -> io_lock` before
    // PageCache snapshots and calls `mkclean_page()`, while still keeping
    // invalidate-read so a truncate cannot pass a claimed token. Exercise the
    // production split helpers with an otherwise-token-only backend: bind is
    // inside admission, every snapshot observes released admission, and both
    // snapshot/admission failures select the dedicated post-admission
    // cancellation path exactly once.
    let split_state = Arc::new(PageCacheSubmissionSelftestState::default());
    let split_backend = Arc::new(PageCacheSubmissionSelftestBackend {
        state: split_state.clone(),
        admission_order: PageCacheWritebackAdmissionOrder::InvalidateBeforeAdmission,
        snapshot_phase: PageCacheWritebackSnapshotPhase::AfterAdmission,
    });
    let split_backend_dyn: Arc<dyn PageCacheBackend> = split_backend;
    // This synthetic backend deliberately has no inode owner.  Use shmem so
    // its front-dirty publisher follows the production `page -> inner`
    // transition without asking a nonexistent file mapping for retention.
    // The backend itself still exercises the ext4-shaped split admission and
    // snapshot contract below.
    let split_cache = PageCache::new_shmem(None, Some(split_backend_dyn.clone()));
    let split_page = split_cache.get_or_create_page_zero(0)?;
    let mark_split_dirty = || -> Result<(), SystemError> {
        let mut page_locked = split_page.write();
        page_locked.add_flags(PageFlags::PG_DIRTY);
        split_cache.mark_page_dirty_page_locked(0, &page_locked)
    };
    let snapshot_after_admission = |batch: &mut ClaimedWritebackBatch| {
        if split_state.admission_depth.load(Ordering::Acquire) != 0 {
            split_state
                .snapshotted_while_admitted
                .fetch_add(1, Ordering::Relaxed);
            return Err(SystemError::EIO);
        }
        split_state
            .snapshotted_after_admission
            .fetch_add(1, Ordering::Relaxed);
        PageCacheManager::snapshot_writeback_batch(batch)
    };
    mark_split_dirty()?;
    let split_blocking_submit = match PageCacheManager::claim_and_snapshot_after_admission_with(
        &split_cache,
        &split_backend_dyn,
        0,
        0,
        None,
        || Ok(MMArch::PAGE_SIZE),
        snapshot_after_admission,
    )? {
        WritebackClaimOutcome::Claimed(batch) => matches!(
            PageCacheManager::submit_writeback_batch(batch),
            Ok(WritebackSubmitOutcome::Completed)
        ),
        WritebackClaimOutcome::NoBatch
        | WritebackClaimOutcome::Deferred(_)
        | WritebackClaimOutcome::FailedRecorded(_) => false,
    };
    mark_split_dirty()?;
    let split_snapshot_error = matches!(
        PageCacheManager::claim_and_snapshot_after_admission_with(
            &split_cache,
            &split_backend_dyn,
            0,
            0,
            None,
            || Ok(MMArch::PAGE_SIZE),
            |_batch| {
                if split_state.admission_depth.load(Ordering::Acquire) != 0 {
                    split_state
                        .snapshotted_while_admitted
                        .fetch_add(1, Ordering::Relaxed);
                    return Err(SystemError::EIO);
                }
                split_state
                    .snapshotted_after_admission
                    .fetch_add(1, Ordering::Relaxed);
                Err(SystemError::EIO)
            },
        ),
        Ok(WritebackClaimOutcome::FailedRecorded(SystemError::EIO))
    );
    let split_snapshot_error_redirtied = {
        let inner = split_cache.inner.lock();
        inner
            .get_entry(0)
            .is_some_and(|entry| entry.state() == PageState::Dirty)
            && inner.dirty_pages.contains(&0)
            && !inner.writeback_pages.contains(&0)
    };
    split_state
        .fail_admission_after_claim
        .store(true, Ordering::Release);
    let split_blocking_admission_error = matches!(
        PageCacheManager::claim_and_snapshot_after_admission_with(
            &split_cache,
            &split_backend_dyn,
            0,
            0,
            None,
            || Ok(MMArch::PAGE_SIZE),
            snapshot_after_admission,
        ),
        Ok(WritebackClaimOutcome::FailedRecorded(SystemError::EIO))
    );
    let split_blocking_admission_error_redirtied = {
        let inner = split_cache.inner.lock();
        inner
            .get_entry(0)
            .is_some_and(|entry| entry.state() == PageState::Dirty)
            && inner.dirty_pages.contains(&0)
            && !inner.writeback_pages.contains(&0)
    };
    let split_try_submit = match PageCacheManager::try_claim_and_snapshot_after_admission_with(
        &split_cache,
        &split_backend_dyn,
        0,
        0,
        || Ok(Some(MMArch::PAGE_SIZE)),
        snapshot_after_admission,
    )? {
        WritebackClaimOutcome::Claimed(batch) => matches!(
            PageCacheManager::submit_writeback_batch(batch),
            Ok(WritebackSubmitOutcome::Completed)
        ),
        WritebackClaimOutcome::NoBatch
        | WritebackClaimOutcome::Deferred(_)
        | WritebackClaimOutcome::FailedRecorded(_) => false,
    };
    mark_split_dirty()?;
    split_state
        .fail_admission_after_claim
        .store(true, Ordering::Release);
    let split_try_admission_error = matches!(
        PageCacheManager::try_claim_and_snapshot_after_admission_with(
            &split_cache,
            &split_backend_dyn,
            0,
            0,
            || Ok(Some(MMArch::PAGE_SIZE)),
            snapshot_after_admission,
        ),
        Ok(WritebackClaimOutcome::FailedRecorded(SystemError::EIO))
    );
    let split_try_admission_error_redirtied = {
        let inner = split_cache.inner.lock();
        inner
            .get_entry(0)
            .is_some_and(|entry| entry.state() == PageState::Dirty)
            && inner.dirty_pages.contains(&0)
            && !inner.writeback_pages.contains(&0)
    };
    let split_try_rejected_by_invalidation = {
        let _invalidate = split_cache.invalidate_write();
        matches!(
            PageCacheManager::try_claim_and_snapshot_after_admission_with(
                &split_cache,
                &split_backend_dyn,
                0,
                0,
                || Ok(Some(MMArch::PAGE_SIZE)),
                snapshot_after_admission,
            ),
            Ok(WritebackClaimOutcome::NoBatch)
        )
    };
    let split_try_rejection_keeps_dirty = {
        let inner = split_cache.inner.lock();
        inner
            .get_entry(0)
            .is_some_and(|entry| entry.state() == PageState::Dirty)
            && inner.dirty_pages.contains(&0)
            && !inner.writeback_pages.contains(&0)
    };
    let split_snapshot_phase_ok = split_blocking_submit
        && split_snapshot_error
        && split_snapshot_error_redirtied
        && split_blocking_admission_error
        && split_blocking_admission_error_redirtied
        && split_try_submit
        && split_try_admission_error
        && split_try_admission_error_redirtied
        && split_try_rejected_by_invalidation
        && split_try_rejection_keeps_dirty
        && split_state.bound.load(Ordering::Acquire) == 5
        && split_state.submitted.load(Ordering::Acquire) == 2
        && split_state
            .submitted_after_admission
            .load(Ordering::Acquire)
            == 2
        && split_state.submitted_while_admitted.load(Ordering::Acquire) == 0
        && split_state
            .snapshotted_after_admission
            .load(Ordering::Acquire)
            == 3
        && split_state
            .snapshotted_while_admitted
            .load(Ordering::Acquire)
            == 0
        && split_state.cancelled.load(Ordering::Acquire) == 3
        && split_state.cancelled_while_admitted.load(Ordering::Acquire) == 0
        && split_state
            .cancelled_outside_admission
            .load(Ordering::Acquire)
            == 0
        && split_state
            .cancelled_after_admission
            .load(Ordering::Acquire)
            == 3
        && split_state
            .cancellation_reacquired_admission
            .load(Ordering::Acquire)
            == 3
        && split_state.bound_outside_admission.load(Ordering::Acquire) == 0
        && split_state.private_claims.load(Ordering::Acquire) == 0
        && split_state.admission_depth.load(Ordering::Acquire) == 0
        && split_state.fallback_writes.load(Ordering::Acquire) == 0
        && split_state.generation_regressions.load(Ordering::Acquire) == 0
        && split_state.single_page_certificates.load(Ordering::Acquire) != 0
        && split_state.certificate_errors.load(Ordering::Acquire) == 0;
    let submission_token_ok = submitted_admission
        && submitted
        && token_direct_rejected_before_bind
        && token_direct_rejected_after_lock
        && multi_page_descriptor_ok
        && snapshot_error
        && snapshot_error_page_redirtied
        && bind_error
        && bind_error_batch.is_none()
        && bind_error_page_stayed_dirty
        && submit_error_admission
        && submit_error
        && submit_error_page_redirtied
        && submit_error_recorded
        && submit_error_acknowledged == Some(SystemError::EIO)
        && tagged_snapshot_error
        && tagged_snapshot_error_recorded_once
        && tagged_snapshot_error_not_recorded_twice
        && tagged_snapshot_error_page_redirtied
        && zero_payload_admission
        && zero_payload_retired
        && zero_payload_bypassed
        && zero_payload_generation_unchanged
        && zero_payload_page_retired
        && overflow_generation_rejected
        && overflow_generation_preserves_dirty
        && overflow_removed
        && within_admission_post_claim_error
        && within_admission_post_claim_error_redirtied
        && submission_state.bind_attempts.load(Ordering::Acquire) == 10
        && submission_state.bound.load(Ordering::Acquire) == 10
        && submission_state.submitted.load(Ordering::Acquire) == 3
        && submission_state.failed_submissions.load(Ordering::Acquire) == 1
        && submission_state
            .submitted_after_admission
            .load(Ordering::Acquire)
            == 5
        && submission_state
            .submitted_while_admitted
            .load(Ordering::Acquire)
            == 0
        && submission_state.cancelled.load(Ordering::Acquire) == 3
        && submission_state
            .cancelled_while_admitted
            .load(Ordering::Acquire)
            == 2
        && submission_state
            .cancelled_outside_admission
            .load(Ordering::Acquire)
            == 0
        && submission_state
            .cancelled_after_admission_without_invalidate
            .load(Ordering::Acquire)
            == 1
        && submission_state
            .bound_outside_admission
            .load(Ordering::Acquire)
            == 0
        && submission_state.private_claims.load(Ordering::Acquire) == 0
        && submission_state.admission_depth.load(Ordering::Acquire) == 0
        && submission_state.fallback_writes.load(Ordering::Acquire) == 0
        && submission_state
            .generation_regressions
            .load(Ordering::Acquire)
            == 0
        && submission_state
            .single_page_certificates
            .load(Ordering::Acquire)
            != 0
        && submission_state.certificate_errors.load(Ordering::Acquire) == 0
        && split_snapshot_phase_ok;

    // A tagged retry replaces its Dirty epoch tag with a pending submission
    // record before snapshotting.  Hold the snapshot at that exact point and
    // prove a WAIT_BEFORE-style waiter remains blocked; the historical
    // tag-clear -> record gap made this waiter return before the backend path
    // was even eligible to run.
    mark_submission_dirty(0)?;
    let tagged_submission_epoch = 0x5a5b_u64;
    let tagged_submission_entry = {
        let _tagged_writeback = submission_cache.tagged_writeback_lock.lock();
        let inner = submission_cache.inner.lock();
        let entry = inner.get_entry(0).ok_or(SystemError::EIO)?;
        entry.set_writeback_tag(tagged_submission_epoch);
        entry
    };
    let tagged_submission_gate = Arc::new(PageCacheTaggedSubmissionSelftestGate::default());
    let tagged_worker_cache = submission_cache.clone();
    let tagged_worker_backend = submission_backend_dyn.clone();
    let tagged_worker_entry = tagged_submission_entry.clone();
    let tagged_worker_gate = tagged_submission_gate.clone();
    PAGECACHE_WRITEBACK_WQS[0].enqueue(Work::new(move || {
        let mut claimed = WritebackClaimOutcome::NoBatch;
        let admission = PageCacheManager::with_writeback_admission(
            &tagged_worker_cache,
            &tagged_worker_backend,
            &mut || {
                claimed = PageCacheManager::claim_and_snapshot_locked_with(
                    &tagged_worker_cache,
                    0,
                    0,
                    token_file_size,
                    Some((0, &tagged_worker_entry, tagged_submission_epoch)),
                    true,
                    |batch| {
                        tagged_worker_gate
                            .snapshot_entered
                            .store(true, Ordering::Release);
                        tagged_worker_gate.wait.wake_all();
                        tagged_worker_gate.wait.wait_until(|| {
                            tagged_worker_gate
                                .release_snapshot
                                .load(Ordering::Acquire)
                                .then_some(())
                        });
                        PageCacheManager::snapshot_writeback_batch(batch)
                    },
                )?;
                Ok(())
            },
        );
        let submitted = match claimed {
            WritebackClaimOutcome::Claimed(batch) => matches!(
                PageCacheManager::submit_writeback_batch(batch),
                Ok(WritebackSubmitOutcome::Completed)
            ),
            WritebackClaimOutcome::NoBatch
            | WritebackClaimOutcome::Deferred(_)
            | WritebackClaimOutcome::FailedRecorded(_) => false,
        };
        tagged_worker_gate
            .worker_succeeded
            .store(admission.is_ok() && submitted, Ordering::Release);
        tagged_worker_gate
            .worker_finished
            .store(true, Ordering::Release);
        tagged_worker_gate.wait.wake_all();
    }));
    const TRACKED_SUBMISSION_SELFTEST_TIMEOUT: Duration = Duration::from_secs(2);
    const TRACKED_SUBMISSION_PROBE_TIMEOUT: Duration = Duration::from_millis(50);
    let snapshot_entered = tagged_submission_gate
        .wait
        .wait_until_timeout(
            || {
                tagged_submission_gate
                    .snapshot_entered
                    .load(Ordering::Acquire)
                    .then_some(())
            },
            TRACKED_SUBMISSION_SELFTEST_TIMEOUT,
        )
        .is_ok();
    let tagged_waiter_cache = submission_cache.clone();
    let tagged_waiter_gate = tagged_submission_gate.clone();
    if snapshot_entered {
        PAGECACHE_WRITEBACK_WQS[1].enqueue(Work::new(move || {
            tagged_waiter_gate
                .waiter_started
                .store(true, Ordering::Release);
            tagged_waiter_gate.wait.wake_all();
            let completed = PageCacheManager::wait_tagged_writeback_submission(
                &tagged_waiter_cache,
                0,
                0,
                tagged_submission_epoch,
            )
            .is_ok();
            tagged_waiter_gate
                .waiter_finished
                .store(completed, Ordering::Release);
            tagged_waiter_gate.wait.wake_all();
        }));
    }
    let waiter_started = snapshot_entered
        && tagged_submission_gate
            .wait
            .wait_until_timeout(
                || {
                    tagged_submission_gate
                        .waiter_started
                        .load(Ordering::Acquire)
                        .then_some(())
                },
                TRACKED_SUBMISSION_SELFTEST_TIMEOUT,
            )
            .is_ok();
    // Give the waiter a bounded opportunity to return incorrectly while the
    // snapshot gate is still closed. A correct record keeps it asleep.
    let waiter_held_by_record = waiter_started
        && tagged_submission_gate
            .wait
            .wait_until_timeout(
                || {
                    tagged_submission_gate
                        .waiter_finished
                        .load(Ordering::Acquire)
                        .then_some(())
                },
                TRACKED_SUBMISSION_PROBE_TIMEOUT,
            )
            .is_err();
    tagged_submission_gate
        .release_snapshot
        .store(true, Ordering::Release);
    tagged_submission_gate.wait.wake_all();
    let tagged_submission_completed = tagged_submission_gate
        .wait
        .wait_until_timeout(
            || {
                (tagged_submission_gate
                    .worker_finished
                    .load(Ordering::Acquire)
                    && tagged_submission_gate
                        .waiter_finished
                        .load(Ordering::Acquire))
                .then_some(())
            },
            TRACKED_SUBMISSION_SELFTEST_TIMEOUT,
        )
        .is_ok();
    let submission_tracker_ok = snapshot_entered
        && waiter_started
        && waiter_held_by_record
        && tagged_submission_completed
        && tagged_submission_gate
            .worker_succeeded
            .load(Ordering::Acquire);
    let submission_removed = submission_cache.manager.remove_page(0)?.is_some()
        && submission_cache.manager.remove_page(1)?.is_some();
    let submission_paddr0 = submission_page0.phys_address();
    let submission_paddr1 = submission_page1.phys_address();
    page_manager_lock().remove_page(&submission_paddr0);
    let _ = page_reclaimer_lock().remove_page(&submission_paddr0);
    page_manager_lock().remove_page(&submission_paddr1);
    let _ = page_reclaimer_lock().remove_page(&submission_paddr1);
    if !submission_token_ok || !submission_defer_ok || !submission_tracker_ok || !submission_removed
    {
        return Ok(alloc::format!(
            "status=fail stage=writeback_submission_token bind_attempts={} bind={} submit={} failed_submit={} submit_after_admission={} submit_while_admitted={} cancel={} cancel_in_admission={} cancel_outside_admission={} bind_outside_admission={} direct_rejected_before_bind={token_direct_rejected_before_bind} direct_rejected_after_lock={token_direct_rejected_after_lock} deferred_before_claim={} deferred_after_submit={} deferred_waits={} defer_ok={submission_defer_ok} submit_defer_admission={submit_defer_admission} submit_defer_dirty={submit_defer_page_redirtied} submit_defer_preserves_pg_error={submit_defer_preserves_page_error} submit_defer_errseq_clean={submit_defer_no_errseq} submit_defer_errseq_before={errseq_before_submit_defer} submit_defer_errseq_after={errseq_after_submit_defer} submit_defer_waited={submit_defer_waited} submit_defer_retry_admission={submit_defer_retry_admission} submit_defer_retry_completed={submit_defer_retry_completed} submit_defer_retry_clears_pg_error={submit_defer_retry_clears_page_error} claim_defer_admission={claim_defer_admission} claim_defer_untouched={claim_defer_page_untouched} claim_defer_waited={claim_defer_waited} claim_defer_retry_admission={claim_defer_retry_admission} claim_defer_retry_completed={claim_defer_retry_completed} tracker_ok={submission_tracker_ok} tracker_snapshot_entered={snapshot_entered} tracker_waiter_started={waiter_started} tracker_waiter_held={waiter_held_by_record} tracker_completed={tagged_submission_completed} zero_payload_bypassed={zero_payload_bypassed} zero_payload_generation_unchanged={zero_payload_generation_unchanged} zero_payload_page_retired={zero_payload_page_retired} overflow_generation_rejected={overflow_generation_rejected} overflow_generation_preserves_dirty={overflow_generation_preserves_dirty} private_claims={} fallback={} snapshot_error={snapshot_error} snapshot_dirty={snapshot_error_page_redirtied} bind_error={bind_error} bind_error_dirty={bind_error_page_stayed_dirty} submit_error={submit_error} submit_error_dirty={submit_error_page_redirtied} submit_errseq={submit_error_recorded} tagged_snapshot_error={tagged_snapshot_error} tagged_snapshot_dirty={tagged_snapshot_error_page_redirtied} zero_payload={zero_payload_retired} removed={submission_removed}\n",
            submission_state.bind_attempts.load(Ordering::Acquire),
            submission_state.bound.load(Ordering::Acquire),
            submission_state.submitted.load(Ordering::Acquire),
            submission_state.failed_submissions.load(Ordering::Acquire),
            submission_state.submitted_after_admission.load(Ordering::Acquire),
            submission_state.submitted_while_admitted.load(Ordering::Acquire),
            submission_state.cancelled.load(Ordering::Acquire),
            submission_state.cancelled_while_admitted.load(Ordering::Acquire),
            submission_state.cancelled_outside_admission.load(Ordering::Acquire),
            submission_state.bound_outside_admission.load(Ordering::Acquire),
            submission_state.deferred_before_claim.load(Ordering::Acquire),
            submission_state.deferred_after_submit.load(Ordering::Acquire),
            submission_state.deferred_waits.load(Ordering::Acquire),
            submission_state.private_claims.load(Ordering::Acquire),
            submission_state.fallback_writes.load(Ordering::Acquire),
        ));
    }
    drop(submission_cache);

    // Filesystem quota follows exact cache membership, including removal.
    let quota_backend = Arc::new(PageCacheQuotaSelftestBackend::default());
    let quota_cache = PageCache::new_shmem(None, Some(quota_backend.clone()));
    let quota_page = quota_cache.get_or_create_page_zero(0)?;
    let quota_removed = quota_cache.manager.remove_page(0)?.is_some();
    let quota_ok = quota_removed
        && quota_backend.reserved.load(Ordering::Acquire) == 1
        && quota_backend.released.load(Ordering::Acquire) == 1;
    let quota_paddr = quota_page.phys_address();
    page_manager_lock().remove_page(&quota_paddr);
    let _ = page_reclaimer_lock().remove_page(&quota_paddr);
    if !quota_ok {
        return Ok("status=fail stage=filesystem_quota_membership\n".into());
    }

    // Ordinary file membership: insert, explicit remove, and duplicate remove.
    let file_cache = PageCache::new(None, None);
    let file_page = file_cache.get_or_create_page_zero(0)?;
    let file_entry = file_cache
        .inner
        .lock()
        .get_entry(0)
        .ok_or(SystemError::EIO)?;
    let file_ok = file_entry.accounting() == PageEntryAccounting::File
        && file_cache.manager.remove_page(0)?.is_some()
        && file_entry.accounting() == PageEntryAccounting::Unaccounted
        && file_cache.manager.remove_page(0)?.is_none();
    let file_paddr = file_page.phys_address();
    page_manager_lock().remove_page(&file_paddr);
    let _ = page_reclaimer_lock().remove_page(&file_paddr);
    if !file_ok {
        return Ok("status=fail stage=file_membership\n".into());
    }

    // Shmem classification is immutable and follows the entry identity.
    let shmem_cache = PageCache::new_shmem(None, None);
    let shmem_page = shmem_cache.get_or_create_page_zero(0)?;
    let shmem_entry = shmem_cache
        .inner
        .lock()
        .get_entry(0)
        .ok_or(SystemError::EIO)?;
    let shmem_ok = shmem_entry.accounting() == PageEntryAccounting::Shmem
        && shmem_cache.manager.remove_page(0)?.is_some()
        && shmem_entry.accounting() == PageEntryAccounting::Unaccounted;
    let shmem_paddr = shmem_page.phys_address();
    page_manager_lock().remove_page(&shmem_paddr);
    let _ = page_reclaimer_lock().remove_page(&shmem_paddr);
    if !shmem_ok {
        return Ok("status=fail stage=shmem_identity\n".into());
    }

    // Transaction rollback must observe PG_DIRTY even while publication to
    // the cache dirty tag/state is still pending.
    let rollback_cache = PageCache::new_shmem(None, None);
    let rollback_page = rollback_cache.get_or_create_page_zero(0)?;
    rollback_page.write().add_flags(PageFlags::PG_DIRTY);
    let discarded = rollback_cache
        .manager
        .discard_created_page(0, &rollback_page)?;
    let rollback_retained = rollback_cache
        .inner
        .lock()
        .get_entry(0)
        .is_some_and(|entry| Arc::ptr_eq(&entry.page, &rollback_page));
    let rollback_removed = rollback_cache.manager.remove_page(0)?.is_some();
    let rollback_paddr = rollback_page.phys_address();
    page_manager_lock().remove_page(&rollback_paddr);
    let _ = page_reclaimer_lock().remove_page(&rollback_paddr);
    if discarded || !rollback_retained || !rollback_removed {
        return Ok("status=fail stage=dirty_transaction_rollback\n".into());
    }

    // Loading rollback consumes membership once; a late state publication on
    // the detached entry must not revive it.
    let state_cache = PageCache::new(None, None);
    let loading_page = state_cache.allocate_page(Arc::downgrade(&state_cache), 0)?;
    let loading_entry = Arc::new(PageEntry::new(loading_page.clone(), PageState::Loading));
    state_cache
        .inner
        .lock()
        .insert_entry(0, loading_entry.clone())?;
    let loading_removed = state_cache.inner.lock().remove_page(0).is_some();
    loading_entry.account_state_transition(PageState::Loading, PageState::UpToDate);
    loading_entry.set_state(PageState::UpToDate);
    let loading_ok =
        loading_removed && loading_entry.accounting() == PageEntryAccounting::Unaccounted;
    let loading_paddr = loading_page.phys_address();
    page_manager_lock().remove_page(&loading_paddr);
    let _ = page_reclaimer_lock().remove_page(&loading_paddr);
    if !loading_ok {
        return Ok("status=fail stage=loading_rollback\n".into());
    }

    // Exercise the production writeback claim/completion state machine. A
    // successful completion returns to UpToDate; an error completion redirties
    // the same attached entry before normal removal closes the accounting.
    let writeback_cache = PageCache::new(None, None);
    let writeback_page = writeback_cache.get_or_create_page_zero(0)?;
    let writeback_entry = writeback_cache
        .inner
        .lock()
        .get_entry(0)
        .ok_or(SystemError::EIO)?;
    let writeback_paddr = writeback_page.phys_address();
    writeback_page.write().add_flags(PageFlags::PG_DIRTY);
    {
        let mut inner = writeback_cache.inner.lock();
        writeback_entry.account_state_transition(PageState::UpToDate, PageState::Dirty);
        writeback_entry.set_state(PageState::Dirty);
        inner.dirty_pages.insert(0);
    }
    let invalidate_ordinary_claim_rejected = {
        let _invalidate = writeback_cache.invalidate_write();
        !writeback_cache.try_mark_page_writeback(0, writeback_paddr)
    } && writeback_entry.state() == PageState::Dirty
        && writeback_entry.writeback_tag() == 0
        && writeback_entry
            .writeback_incarnation
            .load(Ordering::Acquire)
            == 0
        && {
            let inner = writeback_cache.inner.lock();
            inner.dirty_pages.contains(&0) && !inner.writeback_pages.contains(&0)
        };
    if !invalidate_ordinary_claim_rejected {
        return Ok("status=fail stage=writeback_invalidate_ordinary_claim\n".into());
    }
    writeback_entry.set_writeback_tag(0x6a01);
    let tagged_ordinary_claim_rejected = !writeback_cache
        .try_mark_page_writeback(0, writeback_paddr)
        && writeback_entry.state() == PageState::Dirty
        && writeback_entry.writeback_tag() == 0x6a01
        && writeback_entry
            .writeback_incarnation
            .load(Ordering::Acquire)
            == 0
        && {
            let inner = writeback_cache.inner.lock();
            inner.dirty_pages.contains(&0) && !inner.writeback_pages.contains(&0)
        };
    if !tagged_ordinary_claim_rejected {
        return Ok("status=fail stage=writeback_tagged_ordinary_claim\n".into());
    }
    writeback_entry.set_writeback_tag(0);
    if !writeback_cache.try_mark_page_writeback(0, writeback_paddr) {
        return Ok("status=fail stage=writeback_claim_success\n".into());
    }
    writeback_page.write().remove_flags(PageFlags::PG_DIRTY);
    let first_writeback_incarnation = writeback_entry
        .writeback_incarnation
        .load(Ordering::Acquire);
    {
        let inner = writeback_cache.inner.lock();
        if first_writeback_incarnation == 0
            || !inner.writeback_pages.contains(&0)
            || inner.dirty_pages.contains(&0)
        {
            return Ok("status=fail stage=writeback_set_success\n".into());
        }
    }
    PageCacheManager::finish_writeback_entry_state(
        writeback_cache.clone(),
        0,
        writeback_entry.clone(),
        writeback_page.clone(),
        Ok(()),
        false,
    )?;
    {
        let inner = writeback_cache.inner.lock();
        if writeback_entry.state() != PageState::UpToDate
            || inner.writeback_pages.contains(&0)
            || inner.dirty_pages.contains(&0)
        {
            return Ok("status=fail stage=writeback_complete_success\n".into());
        }
    }
    if !writeback_cache.try_mark_page_writeback(0, writeback_paddr) {
        return Ok("status=fail stage=writeback_claim_error\n".into());
    }
    let second_writeback_incarnation = writeback_entry
        .writeback_incarnation
        .load(Ordering::Acquire);
    if second_writeback_incarnation <= first_writeback_incarnation
        || !matches!(
            PageCacheManager::writeback_incarnation_result(
                &writeback_entry,
                first_writeback_incarnation
            ),
            Some(Ok(()))
        )
    {
        return Ok("status=fail stage=writeback_incarnation_aba\n".into());
    }
    if PageCacheManager::finish_writeback_entry_state(
        writeback_cache.clone(),
        0,
        writeback_entry.clone(),
        writeback_page.clone(),
        Err(SystemError::EIO),
        false,
    )
    .is_ok()
    {
        return Ok("status=fail stage=writeback_error_result\n".into());
    }
    let writeback_removed = {
        let mut inner = writeback_cache.inner.lock();
        if writeback_entry.state() != PageState::Dirty
            || inner.writeback_pages.contains(&0)
            || !inner.dirty_pages.contains(&0)
        {
            return Ok("status=fail stage=writeback_complete_error\n".into());
        }
        inner.remove_page(0).is_some()
    };
    let writeback_ok =
        writeback_removed && writeback_entry.accounting() == PageEntryAccounting::Unaccounted;
    page_manager_lock().remove_page(&writeback_paddr);
    let _ = page_reclaimer_lock().remove_page(&writeback_paddr);
    if !writeback_ok {
        return Ok("status=fail stage=writeback_teardown\n".into());
    }

    // Exhaustion must fail before publishing any Legacy state transition.
    // Wrapping to zero would make a later frozen waiter indistinguishable
    // from an old incarnation.
    let overflow_cache = PageCache::new(None, None);
    let overflow_page = overflow_cache.get_or_create_page_zero(0)?;
    let overflow_entry = overflow_cache
        .inner
        .lock()
        .get_entry(0)
        .ok_or(SystemError::EIO)?;
    overflow_cache.inner.lock().next_writeback_incarnation = u64::MAX;
    if overflow_cache.try_mark_page_writeback(0, overflow_page.phys_address())
        || overflow_entry.state() != PageState::UpToDate
        || overflow_entry.writeback_incarnation.load(Ordering::Acquire) != 0
    {
        return Ok("status=fail stage=writeback_incarnation_overflow\n".into());
    }
    let overflow_removed = overflow_cache.manager.remove_page(0)?.is_some();
    let overflow_paddr = overflow_page.phys_address();
    page_manager_lock().remove_page(&overflow_paddr);
    let _ = page_reclaimer_lock().remove_page(&overflow_paddr);
    if !overflow_removed {
        return Ok("status=fail stage=writeback_incarnation_overflow_cleanup\n".into());
    }

    // Generic asynchronous reads may leave a Loading entry at final drop. A
    // late completion owns only the detached entry and must not revive its
    // mapping accounting or physical manager/reclaimer membership.
    let drop_cache = PageCache::new(None, None);
    let drop_page = drop_cache.allocate_page(Arc::downgrade(&drop_cache), 0)?;
    let drop_paddr = drop_page.phys_address();
    let drop_entry = Arc::new(PageEntry::new(drop_page, PageState::Loading));
    drop_cache
        .inner
        .lock()
        .insert_entry(0, drop_entry.clone())?;
    drop(drop_cache);
    drop_entry.account_state_transition(PageState::Loading, PageState::UpToDate);
    drop_entry.set_state(PageState::UpToDate);
    if drop_entry.accounting() != PageEntryAccounting::Unaccounted
        || page_manager_lock().contains(&drop_paddr)
        || page_reclaimer_lock().get(&drop_paddr).is_some()
    {
        return Ok("status=fail stage=final_drop_loading\n".into());
    }

    let before = pc_stats::snapshot();
    let wiring_cache = PageCache::new_shmem(None, None);
    wiring_cache.set_unevictable(true);
    let mut wiring_pages = Vec::with_capacity(PAGECACHE_ACCOUNTING_SELFTEST_WIRING_PAGES);
    let mut first_wiring_entry = None;
    for index in 0..PAGECACHE_ACCOUNTING_SELFTEST_WIRING_PAGES {
        let page = wiring_cache.get_or_create_page_zero(index)?;
        let mut inner = wiring_cache.inner.lock();
        let entry = inner.get_entry(index).ok_or(SystemError::EIO)?;
        if entry.state() != PageState::UpToDate || !inner.page_indices.contains(&index) {
            return Ok("status=fail stage=dirty_fixture\n".into());
        }
        entry.account_state_transition(PageState::UpToDate, PageState::Dirty);
        entry.set_state(PageState::Dirty);
        inner.dirty_pages.insert(index);
        if !inner.dirty_pages.contains(&index) {
            return Ok("status=fail stage=dirty_set\n".into());
        }
        drop(inner);
        if index == 0 {
            first_wiring_entry = Some(entry);
        }
        wiring_pages.push(page);
    }
    let first_wiring_entry = first_wiring_entry.ok_or(SystemError::EIO)?;
    let first_wiring_paddr = wiring_pages[0].phys_address();
    let unevictable_local_ok = first_wiring_entry
        .accounted_unevictable
        .load(Ordering::Acquire)
        && wiring_pages[0]
            .read()
            .flags()
            .contains(PageFlags::PG_UNEVICTABLE)
        && page_manager_lock().contains(&first_wiring_paddr)
        && page_reclaimer_lock().get(&first_wiring_paddr).is_none();
    if !unevictable_local_ok {
        return Ok("status=fail stage=unevictable_fixture\n".into());
    }
    let dirty_populated = pc_stats::snapshot();
    for (index, page) in wiring_pages.iter().enumerate() {
        if !wiring_cache.try_mark_page_writeback(index, page.phys_address()) {
            return Ok("status=fail stage=writeback_batch_claim\n".into());
        }
    }
    let writeback_populated = pc_stats::snapshot();
    for (index, page) in wiring_pages.iter().enumerate() {
        let entry = wiring_cache
            .inner
            .lock()
            .get_entry(index)
            .ok_or(SystemError::EIO)?;
        PageCacheManager::finish_writeback_entry_state(
            wiring_cache.clone(),
            index,
            entry,
            page.clone(),
            Ok(()),
            false,
        )?;
    }
    {
        let inner = wiring_cache.inner.lock();
        if !inner.writeback_pages.is_empty()
            || !inner.dirty_pages.is_empty()
            || inner
                .pages
                .values()
                .any(|entry| entry.state() != PageState::UpToDate)
        {
            return Ok("status=fail stage=writeback_batch_complete\n".into());
        }
    }
    let writeback_completed = pc_stats::snapshot();
    drop(wiring_cache);
    let after = pc_stats::snapshot();
    let unevictable_drop_local_ok = first_wiring_entry.accounting()
        == PageEntryAccounting::Unaccounted
        && !first_wiring_entry
            .accounted_unevictable
            .load(Ordering::Acquire)
        && !page_manager_lock().contains(&first_wiring_paddr)
        && page_reclaimer_lock().get(&first_wiring_paddr).is_none();
    if !unevictable_drop_local_ok {
        return Ok("status=fail stage=unevictable_drop\n".into());
    }
    drop(wiring_pages);

    let file_insert_delta = dirty_populated.file_pages as i128 - before.file_pages as i128;
    let shmem_insert_delta = dirty_populated.shmem_pages as i128 - before.shmem_pages as i128;
    let dirty_insert_delta = dirty_populated.file_dirty as i128 - before.file_dirty as i128;
    let unevictable_insert_delta = dirty_populated.unevictable as i128 - before.unevictable as i128;
    let writeback_insert_delta =
        writeback_populated.file_writeback as i128 - before.file_writeback as i128;
    let writeback_completion_drift =
        writeback_completed.file_writeback as i128 - before.file_writeback as i128;
    let file_drop_drift = after.file_pages as i128 - before.file_pages as i128;
    let shmem_drop_drift = after.shmem_pages as i128 - before.shmem_pages as i128;
    let dirty_drop_drift = after.file_dirty as i128 - before.file_dirty as i128;
    let unevictable_drop_drift = after.unevictable as i128 - before.unevictable as i128;
    let writeback_drop_drift = after.file_writeback as i128 - before.file_writeback as i128;
    let insert_delta_ok = |delta: i128| {
        (delta - PAGECACHE_ACCOUNTING_SELFTEST_WIRING_PAGES as i128).abs()
            <= PAGECACHE_ACCOUNTING_SELFTEST_WIRING_NOISE
    };
    if !insert_delta_ok(file_insert_delta)
        || !insert_delta_ok(shmem_insert_delta)
        || !insert_delta_ok(dirty_insert_delta)
        || !insert_delta_ok(unevictable_insert_delta)
        || !insert_delta_ok(writeback_insert_delta)
        || writeback_completion_drift.abs() > PAGECACHE_ACCOUNTING_SELFTEST_WIRING_NOISE
        || file_drop_drift.abs() > PAGECACHE_ACCOUNTING_SELFTEST_WIRING_NOISE
        || shmem_drop_drift.abs() > PAGECACHE_ACCOUNTING_SELFTEST_WIRING_NOISE
        || dirty_drop_drift.abs() > PAGECACHE_ACCOUNTING_SELFTEST_WIRING_NOISE
        || unevictable_drop_drift.abs() > PAGECACHE_ACCOUNTING_SELFTEST_WIRING_NOISE
        || writeback_drop_drift.abs() > PAGECACHE_ACCOUNTING_SELFTEST_WIRING_NOISE
    {
        return Ok(alloc::format!(
            "status=fail stage=global_wiring file_insert_delta={file_insert_delta} shmem_insert_delta={shmem_insert_delta} dirty_insert_delta={dirty_insert_delta} unevictable_insert_delta={unevictable_insert_delta} writeback_insert_delta={writeback_insert_delta} writeback_completion_drift={writeback_completion_drift} file_drop_drift={file_drop_drift} shmem_drop_drift={shmem_drop_drift} dirty_drop_drift={dirty_drop_drift} unevictable_drop_drift={unevictable_drop_drift} writeback_drop_drift={writeback_drop_drift}\n"
        ));
    }

    Ok(alloc::format!(
        "status=ok\npreallocated_batch_lifecycle=ok\nfile_membership=ok\nshmem_membership=ok\ndirty_membership=ok\ndirty_incarnation=ok\nwriteback_membership=ok\nwriteback_admission_order=ok\nwriteback_submission_token=ok\nwriteback_defer_progress=ok\nwriteback_budget_retry=ok\nfault_invalidate_retry_order=ok\ntag_scan_chunk_release=ok\nunevictable_membership=ok\ninflight_teardown=ok\nlate_completion=ok\nglobal_wiring=ok\nlayout=ok\nfile_drop_drift={file_drop_drift}\nshmem_drop_drift={shmem_drop_drift}\ndirty_drop_drift={dirty_drop_drift}\nwriteback_drop_drift={writeback_drop_drift}\nunevictable_drop_drift={unevictable_drop_drift}\nentry_size={entry_size}\nbaseline_size={baseline_size}\n"
    ))
}
