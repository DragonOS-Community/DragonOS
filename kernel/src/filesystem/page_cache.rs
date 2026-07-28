use core::{
    mem::{self, ManuallyDrop},
    sync::atomic::{AtomicBool, AtomicU64, AtomicU8, AtomicUsize, Ordering},
};

use alloc::{
    boxed::Box,
    collections::{BTreeSet, VecDeque},
    sync::{Arc, Weak},
    vec::Vec,
};
use hashbrown::{hash_map::Entry, HashMap};
use system_error::SystemError;

use super::vfs::{
    inode_lifecycle::{InodeRetentionGuard, InodeRetentionKind},
    mount::record_writeback_error_for_fs,
    FilePrivateData, IndexNode, WritebackControl,
};
use crate::exception::workqueue::{schedule_work, Work, WorkQueue};
use crate::libs::errseq::{ErrSeq, ErrSeqValue};
use crate::libs::mutex::MutexGuard;
use crate::libs::rwsem::{RwSem, RwSemReadGuard, RwSemWriteGuard};
use crate::libs::spinlock::SpinLock;
use crate::libs::wait_queue::WaitQueue;
use crate::mm::fault::FaultRetryWait;
use crate::mm::page::FileMapInfo;
use crate::mm::page_cache_stats as pc_stats;
use crate::mm::ucontext::LockedVMA;
use crate::sched::completion::Completion;
use crate::time::Duration;
use crate::{arch::mm::LockedFrameAllocator, libs::lazy_init::Lazy};
use crate::{
    arch::MMArch,
    libs::mutex::Mutex,
    mm::{
        mmu_gather::MmuGather,
        page::{page_manager_lock, page_reclaimer_lock, InnerPage, Page, PageFlags},
        ucontext::AddressSpace,
        MemoryManagementArch,
    },
};
use crate::{libs::align::page_align_up, mm::page::PageType};
use lazy_static::lazy_static;

static PAGE_CACHE_ID: AtomicUsize = AtomicUsize::new(0);
/// Certificate identities are separate from diagnostic/cache-table ids and
/// are never permitted to wrap: a delayed-allocation ticket may outlive a
/// removed entry or cache until its drain transaction resolves it.
static PAGE_CACHE_INSTANCE_ID: AtomicU64 = AtomicU64::new(1);
static PAGE_CACHE_ENTRY_INSTANCE_ID: AtomicU64 = AtomicU64::new(1);
static PAGE_CACHE_DMA_RESERVATION_ID: AtomicU64 = AtomicU64::new(1);
static PAGE_CACHE_WRITEBACK_TAG_EPOCH: AtomicU64 = AtomicU64::new(1);

const PAGECACHE_IO_WORKERS: usize = 4;
const MAX_ASYNC_WRITEBACK_BATCHES: usize = PAGECACHE_IO_WORKERS * 2;
static PAGECACHE_IO_RR: AtomicUsize = AtomicUsize::new(0);
static PAGECACHE_WRITEBACK_RR: AtomicUsize = AtomicUsize::new(0);
static ASYNC_WRITEBACK_BATCHES: AtomicUsize = AtomicUsize::new(0);
static ASYNC_WRITEBACK_COMPLETIONS: AtomicU64 = AtomicU64::new(0);
static ASYNC_WRITEBACK_WAIT: WaitQueue = WaitQueue::default();
static ASYNC_WRITEBACK_RETRIES: Mutex<VecDeque<Arc<AsyncWritebackRetryTicket>>> =
    Mutex::new(VecDeque::new());
static PAGECACHE_COMPLETION_SELFTEST_RUNNING: AtomicBool = AtomicBool::new(false);
static PAGECACHE_ACCOUNTING_SELFTEST_RUNNING: AtomicBool = AtomicBool::new(false);

// A batch large enough to dominate normal background noise verifies the
// page-cache VM counters, including the final-drop path that regressed. The
// tolerance avoids treating the global snapshot as an exact local oracle.
const PAGECACHE_ACCOUNTING_SELFTEST_WIRING_PAGES: usize = 128;
const PAGECACHE_ACCOUNTING_SELFTEST_WIRING_NOISE: i128 = 16;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
enum PageCacheKind {
    File = 1,
    Shmem = 2,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PageCacheWritebackProtocol {
    /// The mapping uses only the established `write_pages()` path.
    Legacy,
    /// Every non-empty descriptor uses an ordered submission token or a
    /// claim-time defer ticket; no Legacy batch may overtake that queue.
    Token,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
enum PageCacheWritebackProtocolState {
    Unset = 0,
    Legacy = 1,
    Token = 2,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
enum PageEntryAccounting {
    Unaccounted = 0,
    File = 1,
    Shmem = 2,
}

/// How a file-backed page joined the currently visible dirty incarnation.
///
/// A writeback descriptor generation identifies one later Dirty -> Writeback
/// claim.  It must not be used as the identity of an earlier front-end dirty
/// event: a page can be redirtied while an older claim is in flight.  The
/// delayed-allocation bridge will eventually consume this value together with
/// its opaque `PageEntry` identity, rather than counting write syscalls.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub(crate) enum PageCacheDirtyTransitionKind {
    /// A clean/up-to-date (or recovered) page started a new dirty lifetime.
    NewlyDirty = 1,
    /// A write raced with an older writeback and started the successor
    /// incarnation without changing the visible `Writeback` state.
    RedirtiedDuringWriteback = 2,
    /// Another writer modified a page which was already dirty; it belongs to
    /// the existing incarnation.
    MergedIntoDirty = 3,
}

impl PageCacheDirtyTransitionKind {
    const fn from_started_u8(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::NewlyDirty),
            2 => Some(Self::RedirtiedDuringWriteback),
            _ => None,
        }
    }
}

/// Copy-only identity of the exact front dirty incarnation a future
/// filesystem ticket intends to bind.  It deliberately contains no Arc or
/// filesystem state: a callback under `page -> inner` can publish it without
/// allocation or locking, and PageCache later revalidates it while binding
/// Dirty -> Writeback.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PageCacheDirtyCertificate {
    cache_instance_id: u64,
    entry_instance_id: u64,
    page_index: usize,
    dirty_incarnation: u64,
    kind: PageCacheDirtyTransitionKind,
}

impl PageCacheDirtyCertificate {
    pub(crate) const fn cache_instance_id(&self) -> u64 {
        self.cache_instance_id
    }

    pub(crate) const fn entry_instance_id(&self) -> u64 {
        self.entry_instance_id
    }

    pub(crate) const fn page_index(&self) -> usize {
        self.page_index
    }

    pub(crate) const fn dirty_incarnation(&self) -> u64 {
        self.dirty_incarnation
    }

    pub(crate) const fn kind(&self) -> PageCacheDirtyTransitionKind {
        self.kind
    }
}

/// A Copy-only proof that PageCache merged a front write into an existing dirty
/// incarnation while holding its `page -> inner` publication critical section.
///
/// Its fields are deliberately private to this module.  A future filesystem
/// bridge may inspect its provenance, but cannot construct a value that merely
/// resembles a PageCache-local merge event and use it to release an external
/// reservation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PageCacheDirtyMergeProvenance {
    cache_instance_id: u64,
    entry_instance_id: u64,
    page_index: usize,
    dirty_incarnation: u64,
}

impl PageCacheDirtyMergeProvenance {
    pub(crate) const fn page_index(&self) -> usize {
        self.page_index
    }

    pub(crate) const fn dirty_incarnation(&self) -> u64 {
        self.dirty_incarnation
    }
}

/// Result of attempting to publish a front-end dirty event.
///
/// Only `Started` owns a capability that a future filesystem bridge may bind
/// to a ticket.  A `Merged` event intentionally has no `PageEntry` ownership,
/// so repeated writes to an already-dirty page cannot manufacture duplicate
/// tickets for the same dirty incarnation.  Its provenance is non-forgeable
/// outside this module even though consumers receive only Copy metadata.
#[derive(Debug)]
pub(crate) enum PageCacheDirtyTransition {
    Started(PageCacheDirtyIncarnation),
    Merged(PageCacheDirtyMergeProvenance),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PageCacheExpectedDirtyTransition {
    Start,
    Merge(PageCacheDirtyCertificate),
}

/// Internal result of publishing a front-end dirty event.
///
/// This deliberately carries only plain metadata.  Normal buffered-write and
/// fault paths do not yet have a delayed-allocation consumer, so making them
/// clone `Arc<PageEntry>` merely to drop an unused capability would add a
/// refcount operation to every newly-dirty/redirty page.  The explicit
/// transition API below turns `Started` into the linear capability only when
/// a future audited bridge asks for it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PageCacheDirtyPublication {
    Started {
        page_index: usize,
        dirty_incarnation: u64,
        kind: PageCacheDirtyTransitionKind,
    },
    Merged {
        page_index: usize,
        dirty_incarnation: u64,
    },
}

impl PageCacheDirtyPublication {
    fn into_transition(
        self,
        cache_instance_id: u64,
        entry: &Arc<PageEntry>,
    ) -> PageCacheDirtyTransition {
        match self {
            Self::Started {
                page_index,
                dirty_incarnation,
                kind,
            } => PageCacheDirtyTransition::Started(PageCacheDirtyIncarnation {
                entry: entry.clone(),
                certificate: PageCacheDirtyCertificate {
                    cache_instance_id,
                    entry_instance_id: entry.instance_id(),
                    page_index,
                    dirty_incarnation,
                    kind,
                },
            }),
            Self::Merged {
                page_index,
                dirty_incarnation,
            } => PageCacheDirtyTransition::Merged(PageCacheDirtyMergeProvenance {
                cache_instance_id,
                entry_instance_id: entry.instance_id(),
                page_index,
                dirty_incarnation,
            }),
        }
    }
}

impl PageCacheDirtyTransition {
    pub(crate) const fn kind(&self) -> PageCacheDirtyTransitionKind {
        match self {
            Self::Started(incarnation) => incarnation.kind(),
            Self::Merged(_) => PageCacheDirtyTransitionKind::MergedIntoDirty,
        }
    }

    pub(crate) const fn page_index(&self) -> usize {
        match self {
            Self::Started(incarnation) => incarnation.page_index(),
            Self::Merged(provenance) => provenance.page_index(),
        }
    }

    pub(crate) const fn dirty_incarnation(&self) -> u64 {
        match self {
            Self::Started(incarnation) => incarnation.dirty_incarnation(),
            Self::Merged(provenance) => provenance.dirty_incarnation(),
        }
    }

    /// Only a started front dirty lifetime may bind a filesystem ticket.
    /// Merged writes intentionally have no independent certificate.
    pub(crate) const fn certificate(&self) -> Option<PageCacheDirtyCertificate> {
        match self {
            Self::Started(incarnation) => Some(incarnation.certificate()),
            Self::Merged(_) => None,
        }
    }
}

/// Opaque identity of one newly-started front-end dirty incarnation.
///
/// Holding the private `Arc<PageEntry>` makes the identity non-forgeable and
/// prevents an entry removal/reuse ABA while a future filesystem ticket keeps
/// this transition.  It intentionally carries no filesystem reservation or
/// writeback generation: those are bound later by the single audited bridge.
#[derive(Debug)]
pub(crate) struct PageCacheDirtyIncarnation {
    entry: Arc<PageEntry>,
    certificate: PageCacheDirtyCertificate,
}

impl PageCacheDirtyIncarnation {
    pub(crate) const fn page_index(&self) -> usize {
        self.certificate.page_index()
    }

    pub(crate) const fn dirty_incarnation(&self) -> u64 {
        self.certificate.dirty_incarnation()
    }

    pub(crate) const fn kind(&self) -> PageCacheDirtyTransitionKind {
        self.certificate.kind()
    }

    pub(crate) const fn certificate(&self) -> PageCacheDirtyCertificate {
        self.certificate
    }
}

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
        match (
            descriptor.first_index() == descriptor.last_index(),
            descriptor.dirty_certificate(),
        ) {
            (true, Some(certificate))
                if certificate.page_index() == descriptor.first_index()
                    && certificate.cache_instance_id() != 0
                    && certificate.entry_instance_id() != 0
                    && certificate.dirty_incarnation() != 0
                    && matches!(
                        certificate.kind(),
                        PageCacheDirtyTransitionKind::NewlyDirty
                            | PageCacheDirtyTransitionKind::RedirtiedDuringWriteback
                    ) =>
            {
                self.state
                    .single_page_certificates
                    .fetch_add(1, Ordering::Relaxed);
            }
            (false, None) => {}
            _ => {
                self.state
                    .certificate_errors
                    .fetch_add(1, Ordering::Relaxed);
            }
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

#[derive(Debug, Default)]
struct FileVmaIndex {
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

lazy_static! {
    static ref PAGECACHE_IO_WQS: Vec<Arc<WorkQueue>> = {
        let mut wqs = Vec::new();
        for i in 0..PAGECACHE_IO_WORKERS {
            wqs.push(WorkQueue::new(&format!("pagecache-io-{i}")));
        }
        wqs
    };
    // Keep completion of already-published Writeback pages independent from
    // generic page-cache work. In particular, host invalidation runs on the
    // generic pool and may hold the filesystem admission barrier while waiting
    // for Writeback. Sharing a FIFO worker could strand the corresponding
    // writeback work behind that waiter and deadlock permanently.
    static ref PAGECACHE_WRITEBACK_WQS: Vec<Arc<WorkQueue>> = {
        let mut wqs = Vec::new();
        for i in 0..PAGECACHE_IO_WORKERS {
            wqs.push(WorkQueue::new(&format!("pagecache-wb-{i}")));
        }
        wqs
    };
    static ref PAGECACHE_REGISTRY: SpinLock<Vec<Weak<PageCache>>> = SpinLock::new(Vec::new());
}

pub(crate) fn schedule_pagecache_io(work: Arc<Work>) {
    let idx = PAGECACHE_IO_RR.fetch_add(1, Ordering::Relaxed) % PAGECACHE_IO_WQS.len();
    PAGECACHE_IO_WQS[idx].enqueue(work);
}

fn schedule_pagecache_writeback(work: Arc<Work>) {
    let idx =
        PAGECACHE_WRITEBACK_RR.fetch_add(1, Ordering::Relaxed) % PAGECACHE_WRITEBACK_WQS.len();
    PAGECACHE_WRITEBACK_WQS[idx].enqueue(work);
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

    let fault_invalidate_retry_order = run_invalidate_retry_lock_order_selftest().unwrap_or(false);
    if !fault_invalidate_retry_order {
        return Ok("status=fail stage=fault_invalidate_retry_order\n".into());
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
    if !writeback_cache.try_mark_page_writeback(0, writeback_paddr) {
        return Ok("status=fail stage=writeback_claim_success\n".into());
    }
    {
        let inner = writeback_cache.inner.lock();
        if !inner.writeback_pages.contains(&0) || inner.dirty_pages.contains(&0) {
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
        "status=ok\nfile_membership=ok\nshmem_membership=ok\ndirty_membership=ok\ndirty_incarnation=ok\nwriteback_membership=ok\nwriteback_admission_order=ok\nwriteback_submission_token=ok\nwriteback_defer_progress=ok\nwriteback_budget_retry=ok\nfault_invalidate_retry_order=ok\nunevictable_membership=ok\ninflight_teardown=ok\nlate_completion=ok\nglobal_wiring=ok\nlayout=ok\nfile_drop_drift={file_drop_drift}\nshmem_drop_drift={shmem_drop_drift}\ndirty_drop_drift={dirty_drop_drift}\nwriteback_drop_drift={writeback_drop_drift}\nunevictable_drop_drift={unevictable_drop_drift}\nentry_size={entry_size}\nbaseline_size={baseline_size}\n"
    ))
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
fn run_async_writeback_budget_retry_selftest() -> bool {
    if !ASYNC_WRITEBACK_RETRIES.lock().is_empty()
        || ASYNC_WRITEBACK_BATCHES.load(Ordering::Acquire) != 0
    {
        return false;
    }

    let mut permits = Vec::with_capacity(MAX_ASYNC_WRITEBACK_BATCHES);
    for _ in 0..MAX_ASYNC_WRITEBACK_BATCHES {
        let Some(permit) = AsyncWritebackPermit::try_acquire() else {
            drop(permits);
            return false;
        };
        permits.push(permit);
    }

    let cache = PageCache::new(None, None);
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

    let drop_cache = PageCache::new(None, None);
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

fn register_page_cache(cache: &Arc<PageCache>) {
    PAGECACHE_REGISTRY
        .lock_irqsave()
        .push(Arc::downgrade(cache));
}

pub fn list_page_caches() -> Vec<Arc<PageCache>> {
    let mut guard = PAGECACHE_REGISTRY.lock_irqsave();
    let mut caches = Vec::new();
    guard.retain(|weak| {
        if let Some(cache) = weak.upgrade() {
            caches.push(cache);
            true
        } else {
            false
        }
    });
    caches
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
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PageCacheWritebackDescriptor {
    first_index: usize,
    last_index: usize,
    file_size: usize,
    valid_bytes: usize,
    writeback_generation: u64,
    /// Present only for the single-page Token prototype. It identifies the
    /// exact front Dirty incarnation which this descriptor is freezing, not
    /// merely the page index or later writeback generation.
    dirty_certificate: Option<PageCacheDirtyCertificate>,
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

    pub(crate) const fn dirty_certificate(&self) -> Option<PageCacheDirtyCertificate> {
        self.dirty_certificate
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

/// 页面缓存
#[derive(Debug)]
pub struct PageCache {
    id: usize,
    /// Never-reused certificate identity. `id` above remains the historical
    /// diagnostic/registry key and may wrap, so it must not escape into a
    /// delayed-writeback capability.
    instance_id: u64,
    inner: Mutex<InnerPageCache>,
    inode: Lazy<Weak<dyn IndexNode>>,
    backend: Lazy<Arc<dyn PageCacheBackend>>,
    i_mmap_rwsem: RwSem<()>,
    invalidate_lock: RwSem<()>,
    file_vma_seq: AtomicU64,
    file_vmas: SpinLock<FileVmaIndex>,
    writeback_error: ErrSeq,
    unevictable: AtomicBool,
    kind: PageCacheKind,
    reclassify_lock: Mutex<()>,
    tagged_writeback_lock: Mutex<()>,
    /// Sequence/event pair for an in-progress tagged writeback operation.
    /// A deferred token returns its pages to Dirty, so `WAIT_AFTER` cannot use
    /// the Writeback set alone as its completion predicate.
    tagged_writeback_progress: AtomicU64,
    tagged_writeback_wait: WaitQueue,
    /// Tagged batches claimed as Writeback but whose worker has not yet
    /// completed its PageCache writeback transition and published its errseq
    /// result. This is separate from the Dirty tag: WAIT_BEFORE|WRITE must
    /// distinguish an already published Writeback page from a batch still
    /// executing on a worker or waiting to publish its completion.
    tagged_writeback_submissions: Mutex<Vec<TaggedWritebackSubmission>>,
    /// At most one global-budget retry per frozen generation.  Keeping the
    /// ticket in the cache makes deferred retry memory proportional to live
    /// tagged generations rather than to repeated WRITE syscalls; Drop also
    /// removes its tickets from the global FIFO immediately.
    tagged_writeback_budget_retries: Mutex<HashMap<u64, TaggedWritebackBudgetRetry>>,
    /// Non-empty backend descriptors are permanently either Legacy or token
    /// based for one mapping.  Mixing the two would let a legacy batch bypass
    /// the ordered delayed-allocation queue.
    writeback_protocol: AtomicU8,
    reclaimer_writeback_active: AtomicBool,
    manager: PageCacheManager,
}

#[derive(Debug)]
struct TaggedWritebackSubmission {
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
struct TaggedWritebackBudgetRetry {
    ticket: Arc<AsyncWritebackRetryTicket>,
    inode: Option<Weak<dyn IndexNode>>,
    start_index: usize,
    frozen_end: usize,
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

pub struct PageDirtyReservation {
    cache: Weak<PageCache>,
    active: bool,
}

impl Drop for PageDirtyReservation {
    fn drop(&mut self) {
        if self.active {
            if let Some(cache) = self.cache.upgrade() {
                cache.cancel_page_dirty_reservation();
            }
        }
    }
}

#[derive(Debug)]
pub struct InnerPageCache {
    #[allow(unused)]
    id: usize,
    pages: HashMap<usize, Arc<PageEntry>>,
    page_indices: BTreeSet<usize>,
    dirty_pages: BTreeSet<usize>,
    /// Page indices whose entries are currently in `PageState::Writeback`.
    ///
    /// This is maintained under the same `inner` lock as the entry state so
    /// range completion checks remain atomic without scanning every cached
    /// page while holding the lock.
    writeback_pages: BTreeSet<usize>,
    /// Next identity assigned to any Dirty -> Writeback transition. Legacy
    /// I/O needs this identity too, so it is distinct from token descriptor
    /// generations.
    next_writeback_incarnation: u64,
    /// Mapping-local identity for ordered writeback descriptors. Allocation
    /// is serialized by `inner`, so an atomic counter would only add a hot
    /// path RMW without adding concurrency protection.
    next_writeback_generation: u64,
    /// Aggregated semantic owner for all dirty and writeback pages in this mapping.
    dirty_retention: Option<InodeRetentionGuard>,
    dirty_preparations: usize,
    kind: PageCacheKind,
    page_cache_ref: Weak<PageCache>,
    accounting_backend: Option<Arc<dyn PageCacheBackend>>,
}

/// 描述一次从页缓存到目标缓冲区的拷贝
pub struct CopyItem {
    entry: Arc<PageEntry>,
    _pin: PageEntryPin,
    page_index: usize,
    page_offset: usize,
    sub_len: usize,
}

#[derive(Debug)]
pub struct PageIoWaiter {
    completion: Completion,
    result: SpinLock<Option<Result<usize, SystemError>>>,
}

impl PageIoWaiter {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            completion: Completion::new(),
            result: SpinLock::new(None),
        })
    }

    pub fn complete(&self, result: Result<usize, SystemError>) {
        *self.result.lock_irqsave() = Some(result);
        self.completion.complete();
    }

    pub fn wait(&self) -> Result<usize, SystemError> {
        self.completion.wait_for_completion()?;
        match self.result.lock_irqsave().as_ref() {
            Some(Ok(len)) => Ok(*len),
            Some(Err(e)) => Err(e.clone()),
            None => Err(SystemError::EIO),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PageState {
    Loading = 0,
    UpToDate = 1,
    Dirty = 2,
    Writeback = 3,
    Error = 4,
}

impl PageState {
    fn is_ready(self) -> bool {
        matches!(
            self,
            PageState::UpToDate | PageState::Dirty | PageState::Writeback
        )
    }
}

struct PageEntry {
    page: Arc<Page>,
    /// Never-reused identity for a certificate. Physical-page addresses and
    /// cache indices can both be reused after reclaim, so neither is enough
    /// to bind a delayed-writeback ticket across teardown races.
    instance_id: u64,
    state: AtomicU8,
    /// Monotonically advances only when a front-end writer starts a new dirty
    /// lifetime.  It is deliberately independent of writeback descriptor
    /// generation: Writeback -> Dirty redirty creates a successor here while
    /// the older descriptor is still in flight.
    /// Front-end dirty lifetime for this entry.  Access is serialized by
    /// `inner`; this remains atomic solely because the entry is `Arc`-shared
    /// with completion and wait paths, and Rust cannot express that external
    /// lock invariant in the field type without an unsafe cell.
    dirty_incarnation: AtomicU64,
    /// The kind associated with the current nonzero dirty incarnation. It is
    /// updated under `inner` alongside `dirty_incarnation`; the atomic exists
    /// only because PageEntry is Arc-shared like the incarnation itself.
    dirty_transition_kind: AtomicU8,
    writeback_tag: AtomicU64,
    /// Identity of the current (or most recently completed) Writeback
    /// incarnation. It is published under `InnerPageCache::inner`.
    writeback_incarnation: AtomicU64,
    accounting: AtomicU8,
    accounted_unevictable: AtomicBool,
    active_users: AtomicUsize,
    wait_queue: WaitQueue,
}

struct PageWritebackRetryWait {
    entry: Arc<PageEntry>,
}

impl core::fmt::Debug for PageWritebackRetryWait {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PageWritebackRetryWait").finish()
    }
}

impl FaultRetryWait for PageWritebackRetryWait {
    fn wait(&self) -> Result<(), SystemError> {
        PageCacheManager::wait_writeback_entry(self.entry.clone())
    }
}

/// Fault-side wait token for a queued PageCache invalidation writer.
///
/// The x86 fault handler holds `AddressSpace::write()` while it reaches a
/// file-backed mapping. It must never sleep acquiring `invalidate_read()`
/// there: writeback can hold that read guard while `mkclean_page()` waits for
/// the fault's AddressSpace write guard, and a queued buffered writer then
/// completes a three-way cycle. The fault instead returns `VM_FAULT_RETRY`,
/// drops its AddressSpace guard, and waits here before retrying.
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

impl core::fmt::Debug for PageEntry {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PageEntry")
            .field("state", &self.state())
            .field("paddr", &self.page.phys_address())
            .finish()
    }
}

#[derive(Clone)]
pub struct PageCacheManager {
    owner: Weak<PageCache>,
}

/// Lifecycle of pages reserved as direct DMA destinations for a cache read.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum PageCacheReadDmaState {
    Prepared = 0,
    Submitted = 1,
    Completed = 2,
    ResetRetired = 3,
}

/// Immutable identity of one full-page DMA output segment.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PageCacheReadDmaDescriptor {
    page_index: usize,
    vaddr: crate::mm::VirtAddr,
    paddr: crate::mm::PhysAddr,
}

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

struct ClaimedWritebackBatch {
    cache: Arc<PageCache>,
    backend: Option<Arc<dyn PageCacheBackend>>,
    first_index: usize,
    descriptor: PageCacheWritebackDescriptor,
    submission: Option<Box<dyn PageCacheWritebackSubmission>>,
    retry_writeback_tag: Option<u64>,
    entries: Vec<(usize, Arc<PageEntry>, Arc<Page>)>,
    guards: Vec<WritebackGuard>,
    data: Vec<u8>,
}

/// Internal result of inspecting one dirty run.  A deferred claim has not
/// altered any page state, while a deferred submission has already restored
/// its claimed batch to Dirty.
enum WritebackClaimOutcome {
    NoBatch,
    Claimed(ClaimedWritebackBatch),
    Deferred(Arc<dyn PageCacheWritebackProgress>),
    /// A batch entered Writeback, then PageCache completed it back to Dirty
    /// and published this terminal error exactly once.  Callers which own a
    /// frozen tagged generation must retire its remaining tags without
    /// recording the error again.
    FailedRecorded(SystemError),
}

enum WritebackSubmitOutcome {
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

/// A frozen dirty-generation started by `start_writeback_range()`.
///
/// Linux's `SYNC_FILE_RANGE_WRITE` and `WB_SYNC_ALL` paths first mark the
/// pages that belonged to the request, then submit only that generation.  A
/// deferred delayed-allocation token temporarily makes such a page Dirty
/// again, so `WAIT_AFTER` must wait for this handle rather than treating an
/// empty Writeback set as completion.  The handle never follows a later dirty
/// generation.
#[derive(Clone)]
pub struct PageCacheWritebackRange {
    cache: Weak<PageCache>,
    start_index: usize,
    /// Highest page index which was Dirty or Writeback while the range was
    /// frozen. Later append pages are outside this operation even when the
    /// caller supplied `usize::MAX`.
    frozen_end: Option<usize>,
    /// Last Dirty -> Writeback incarnation published before the freeze.
    /// This distinguishes pre-existing I/O from a later writeback of the
    /// same cached page.
    writeback_frontier: u64,
    epoch: u64,
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
    fn new(owner: Weak<PageCache>) -> Self {
        Self { owner }
    }

    fn upgrade(&self) -> Result<Arc<PageCache>, SystemError> {
        self.owner.upgrade().ok_or(SystemError::EIO)
    }

    pub fn commit_page(&self, page_index: usize) -> Result<Arc<Page>, SystemError> {
        self.upgrade()?.get_or_create_page_for_read(page_index)
    }

    /// Reserve absent cache indices as full-page DMA destinations.
    ///
    /// Existing pages, including another Loading entry, are never replaced.
    /// The caller must hold this cache's invalidate read guard. Keeping that
    /// ownership at the fill-operation boundary avoids recursively acquiring
    /// the writer-preferring invalidate semaphore while an invalidator waits.
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

    pub fn commit_page_pinned(&self, page_index: usize) -> Result<PageCachePagePin, SystemError> {
        self.upgrade()?
            .get_or_create_page_for_read_pinned(page_index)
    }

    pub fn commit_page_with<F>(&self, page_index: usize, fill: F) -> Result<Arc<Page>, SystemError>
    where
        F: FnOnce(usize, &mut [u8]) -> Result<usize, SystemError>,
    {
        self.upgrade()?.get_or_create_page_with(page_index, fill)
    }

    pub fn commit_page_for_write_with<F>(
        &self,
        page_index: usize,
        fill: F,
    ) -> Result<Arc<Page>, SystemError>
    where
        F: FnOnce(usize, &mut [u8]) -> Result<usize, SystemError>,
    {
        self.upgrade()?
            .get_or_create_page_for_write_with(page_index, fill)
    }

    pub fn commit_overwrite(&self, page_index: usize) -> Result<Arc<Page>, SystemError> {
        self.upgrade()?.get_or_create_page_zero(page_index)
    }

    pub fn commit_overwrite_for_write(&self, page_index: usize) -> Result<Arc<Page>, SystemError> {
        self.upgrade()?
            .get_or_create_page_for_write_with(page_index, |_idx, dst| {
                dst.fill(0);
                Ok(MMArch::PAGE_SIZE)
            })
    }

    pub fn commit_overwrite_pinned(
        &self,
        page_index: usize,
    ) -> Result<PageCachePagePin, SystemError> {
        self.upgrade()?.get_or_create_page_zero_pinned(page_index)
    }

    pub fn commit_overwrite_pinned_with_status(
        &self,
        page_index: usize,
    ) -> Result<(PageCachePagePin, bool), SystemError> {
        self.upgrade()?
            .get_or_create_page_zero_pinned_with_status(page_index)
    }

    /// Count cache holes in an inclusive page-index range from one locked
    /// membership snapshot. This is used only for fallible transaction
    /// metadata sizing; the later per-page insertion remains authoritative.
    pub fn missing_pages_in_range(&self, first: usize, last: usize) -> Result<usize, SystemError> {
        let cache = self.upgrade()?;
        let total = last
            .checked_sub(first)
            .and_then(|count| count.checked_add(1))
            .ok_or(SystemError::ENOMEM)?;
        let present = cache.lock().page_indices.range(first..=last).count();
        total.checked_sub(present).ok_or(SystemError::EIO)
    }

    pub fn prefetch_page(&self, page_index: usize) -> Result<(), SystemError> {
        self.upgrade()?.start_async_read(page_index)
    }

    pub fn decommit_page(&self, page_index: usize) -> Result<(), SystemError> {
        self.writeback_page(page_index)?;
        self.invalidate_range(page_index, page_index)?;
        Ok(())
    }

    pub fn peek_page(&self, page_index: usize) -> Option<Arc<Page>> {
        self.upgrade()
            .ok()
            .and_then(|cache| cache.get_ready_page(page_index))
    }

    pub fn peek_page_pinned(&self, page_index: usize) -> Option<PageCachePagePin> {
        self.upgrade()
            .ok()
            .and_then(|cache| cache.get_ready_page_pinned(page_index))
    }

    pub fn get_page_any(&self, page_index: usize) -> Option<Arc<Page>> {
        self.upgrade().ok().and_then(|cache| {
            let inner = cache.inner.lock();
            inner
                .get_entry(page_index)
                .filter(|entry| entry.state() != PageState::Loading)
                .map(|entry| entry.page.clone())
        })
    }

    pub fn update_clean_page(
        &self,
        page_index: usize,
        page_offset: usize,
        data: &[u8],
    ) -> Result<bool, SystemError> {
        if data.is_empty() {
            return Ok(false);
        }
        match page_offset.checked_add(data.len()) {
            Some(end) if end <= MMArch::PAGE_SIZE => {}
            _ => return Err(SystemError::EINVAL),
        }

        let cache = self.upgrade()?;
        let Some(entry) = cache.inner.lock().get_entry(page_index) else {
            return Ok(false);
        };

        loop {
            match entry.state() {
                PageState::Loading => {
                    if entry.wait_ready().is_err() {
                        return Ok(false);
                    }
                    let current = cache.inner.lock().get_entry(page_index);
                    if !matches!(current.as_ref(), Some(current) if Arc::ptr_eq(current, &entry)) {
                        return Ok(false);
                    }
                    continue;
                }
                PageState::Error | PageState::Dirty | PageState::Writeback => return Ok(false),
                PageState::UpToDate => {
                    let current = cache.inner.lock().get_entry(page_index);
                    if !matches!(current.as_ref(), Some(current) if Arc::ptr_eq(current, &entry)) {
                        return Ok(false);
                    }
                    let mut guard = entry.page.write();
                    if guard
                        .flags()
                        .intersects(PageFlags::PG_DIRTY | PageFlags::PG_WRITEBACK)
                    {
                        return Ok(false);
                    }
                    let dst = unsafe { guard.as_slice_mut() };
                    dst[page_offset..page_offset + data.len()].copy_from_slice(data);
                    guard.add_flags(PageFlags::PG_UPTODATE);
                    return Ok(true);
                }
            }
        }
    }

    /// Merge data into an existing ready cache page.
    ///
    /// This waits for an in-flight writeback before copying, but callers that need
    /// backend write ordering must still hold the page cache invalidate write lock
    /// around their full backend-write and cache-merge sequence.
    pub fn update_ready_page(
        &self,
        page_index: usize,
        page_offset: usize,
        data: &[u8],
    ) -> Result<bool, SystemError> {
        if data.is_empty() {
            return Ok(false);
        }
        match page_offset.checked_add(data.len()) {
            Some(end) if end <= MMArch::PAGE_SIZE => {}
            _ => return Err(SystemError::EINVAL),
        }

        let cache = self.upgrade()?;

        loop {
            let Some(entry) = cache.inner.lock().get_entry(page_index) else {
                return Ok(false);
            };

            match entry.state() {
                PageState::Loading => {
                    if entry.wait_ready().is_err() {
                        return Ok(false);
                    }
                    continue;
                }
                PageState::Writeback => {
                    Self::wait_writeback_entry(entry)?;
                    continue;
                }
                PageState::Error => return Ok(false),
                PageState::UpToDate | PageState::Dirty => {}
            }

            let mut page = entry.page.write();
            match entry.state() {
                PageState::Loading | PageState::Writeback => {
                    drop(page);
                    continue;
                }
                PageState::Error => return Ok(false),
                PageState::UpToDate | PageState::Dirty => {}
            }

            let keep_dirty =
                entry.state() == PageState::Dirty || page.flags().contains(PageFlags::PG_DIRTY);
            // Retention admission may fail.  Obtain it before copying bytes
            // or exposing either page flag, so a rejected dirty merge cannot
            // leave data/PG_DIRTY visible without dirty-set ownership.
            let mut reservation = keep_dirty.then(|| cache.prepare_page_dirty()).transpose()?;

            // `prepare_page_dirty()` takes `inner` while this exact page is
            // locked. Revalidate after it returns: a claim can have changed
            // Dirty to Writeback in that interval, but has not yet been able
            // to snapshot the page while we hold this lock.
            let mut inner = cache.inner.lock();
            let Some(current) = inner.get_entry(page_index) else {
                return Ok(false);
            };
            if !Arc::ptr_eq(&current, &entry) || !Arc::ptr_eq(&current.page, &entry.page) {
                return Ok(false);
            }
            match current.state() {
                PageState::Loading | PageState::Writeback => {
                    drop(inner);
                    drop(page);
                    continue;
                }
                PageState::Error => return Ok(false),
                PageState::UpToDate | PageState::Dirty => {}
            }

            let keep_dirty =
                current.state() == PageState::Dirty || page.flags().contains(PageFlags::PG_DIRTY);
            debug_assert_eq!(keep_dirty, reservation.is_some());
            let dst = unsafe { page.as_slice_mut() };
            dst[page_offset..page_offset + data.len()].copy_from_slice(data);
            page.add_flags(PageFlags::PG_UPTODATE);
            if let Some(reservation) = reservation.as_mut() {
                // Keep `page -> inner` through both halves.  No fallible work
                // remains after the bytes or PG_DIRTY become visible.
                page.add_flags(PageFlags::PG_DIRTY);
                let _ = PageCache::publish_prepared_front_dirty_locked(
                    &mut inner,
                    page_index,
                    &current,
                    reservation,
                );
            }

            return Ok(true);
        }
    }

    pub fn page_mkwrite_retry_wait(
        &self,
        page_index: usize,
        page: &Arc<Page>,
    ) -> Option<Arc<dyn FaultRetryWait>> {
        let cache = self.upgrade().ok()?;
        let entry = cache.inner.lock().get_entry(page_index)?;
        if !Arc::ptr_eq(&entry.page, page) || entry.state() != PageState::Writeback {
            return None;
        }
        Some(Arc::new(PageWritebackRetryWait { entry }))
    }

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

    fn claim_next_writeback_batch(
        cache: &Arc<PageCache>,
        start_index: usize,
        end_index: usize,
        file_size: usize,
        required_first: Option<(usize, &Arc<PageEntry>, u64)>,
        bind_submission: bool,
    ) -> Result<WritebackClaimOutcome, SystemError> {
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
        let reported_pages = match backend.as_ref() {
            Some(backend) => backend.write_batch_pages()?,
            None => 1,
        };
        if reported_pages == 0 {
            return Err(SystemError::EIO);
        }
        let batch_pages = reported_pages.min(64);
        let max_data_len = batch_pages
            .checked_mul(MMArch::PAGE_SIZE)
            .ok_or(SystemError::EOVERFLOW)?;

        let mut candidates = Vec::new();
        candidates
            .try_reserve_exact(batch_pages)
            .map_err(|_| SystemError::ENOMEM)?;
        let mut prepared = Vec::new();
        let mut guards = Vec::new();
        let mut data = Vec::new();
        prepared
            .try_reserve_exact(batch_pages)
            .map_err(|_| SystemError::ENOMEM)?;
        guards
            .try_reserve_exact(batch_pages)
            .map_err(|_| SystemError::ENOMEM)?;
        data.try_reserve_exact(max_data_len)
            .map_err(|_| SystemError::ENOMEM)?;

        let (first_index, descriptor, submission) = {
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
                    .unwrap_or(true);
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
            // The generic Token selftest still exercises multi-page
            // submission. A real delayed-allocation backend must force a
            // one-page batch and reject a missing certificate; only that
            // shape has one exact front dirty incarnation to bind today.
            let dirty_certificate = if token_protocol == Some(PageCacheWritebackProtocol::Token)
                && candidates.len() == 1
            {
                let (page_index, entry) = candidates
                    .first()
                    .expect("non-empty token writeback candidate disappeared");
                Some(entry.current_dirty_certificate(cache.instance_id, *page_index)?)
            } else {
                None
            };
            let claim_descriptor = PageCacheWritebackDescriptor {
                first_index,
                last_index,
                file_size,
                valid_bytes,
                writeback_generation,
                dirty_certificate,
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
            (first_index, claim_descriptor, claim_submission)
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
            backend,
            first_index,
            descriptor,
            submission,
            retry_writeback_tag,
            entries: prepared,
            guards,
            data,
        }))
    }

    fn complete_writeback_batch(
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

    fn snapshot_writeback_batch(batch: &mut ClaimedWritebackBatch) -> Result<(), SystemError> {
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
    fn submit_writeback_batch(
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
    fn claim_and_snapshot_locked_with(
        cache: &Arc<PageCache>,
        start_index: usize,
        end_index: usize,
        file_size: usize,
        required_first: Option<(usize, &Arc<PageEntry>, u64)>,
        bind_submission: bool,
        snapshot: impl FnOnce(&mut ClaimedWritebackBatch) -> Result<(), SystemError>,
    ) -> Result<WritebackClaimOutcome, SystemError> {
        let claim = Self::claim_next_writeback_batch(
            cache,
            start_index,
            end_index,
            file_size,
            required_first,
            bind_submission,
        )?;
        Self::snapshot_claimed_batch_with(
            claim,
            snapshot,
            PageCacheWritebackCancellationContext::BeforeSubmitWithAdmission,
        )
    }

    /// Claim and snapshot with the invalidate read lock already held.
    fn claim_and_snapshot_locked(
        cache: &Arc<PageCache>,
        start_index: usize,
        end_index: usize,
        file_size: usize,
        bind_submission: bool,
    ) -> Result<WritebackClaimOutcome, SystemError> {
        Self::claim_and_snapshot_locked_with(
            cache,
            start_index,
            end_index,
            file_size,
            None,
            bind_submission,
            Self::snapshot_writeback_batch,
        )
    }

    fn claim_and_snapshot_tagged_locked(
        cache: &Arc<PageCache>,
        start_index: usize,
        end_index: usize,
        file_size: usize,
        required_entry: &Arc<PageEntry>,
        epoch: u64,
        bind_submission: bool,
    ) -> Result<WritebackClaimOutcome, SystemError> {
        Self::claim_and_snapshot_locked_with(
            cache,
            start_index,
            end_index,
            file_size,
            Some((start_index, required_entry, epoch)),
            bind_submission,
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
    fn with_writeback_admission(
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
    fn try_with_writeback_admission(
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
    fn claim_and_snapshot_after_admission_with(
        cache: &Arc<PageCache>,
        backend: &Arc<dyn PageCacheBackend>,
        start_index: usize,
        end_index: usize,
        required_first: Option<(usize, &Arc<PageEntry>, u64)>,
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
                start_index,
                end_index,
                file_size,
                required_first,
                true,
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
    fn try_claim_and_snapshot_after_admission_with(
        cache: &Arc<PageCache>,
        backend: &Arc<dyn PageCacheBackend>,
        start_index: usize,
        end_index: usize,
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
    ) -> Result<WritebackClaimOutcome, SystemError> {
        let Some(backend) = cache.backend() else {
            let _invalidate = cache.invalidate_read();
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
            return Self::claim_and_snapshot_after_admission_with(
                cache,
                &backend,
                start_index,
                end_index,
                None,
                || backend.stable_writeback_size(inode),
                Self::snapshot_writeback_batch,
            );
        }
        let mut claimed = WritebackClaimOutcome::NoBatch;
        let admission = Self::with_writeback_admission(cache, &backend, &mut || {
            let file_size = backend.stable_writeback_size(inode)?;
            claimed =
                Self::claim_and_snapshot_locked(cache, start_index, end_index, file_size, true)?;
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
        start_index: usize,
        end_index: usize,
        required_entry: &Arc<PageEntry>,
        epoch: u64,
    ) -> Result<WritebackClaimOutcome, SystemError> {
        let Some(backend) = cache.backend() else {
            let _invalidate = cache.invalidate_read();
            let file_size = inode.metadata()?.size.max(0) as usize;
            return Self::claim_and_snapshot_tagged_locked(
                cache,
                start_index,
                end_index,
                file_size,
                required_entry,
                epoch,
                false,
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
                start_index,
                end_index,
                Some((start_index, required_entry, epoch)),
                || backend.stable_writeback_size(inode),
                Self::snapshot_writeback_batch,
            );
        }
        let mut claimed = WritebackClaimOutcome::NoBatch;
        let admission = Self::with_writeback_admission(cache, &backend, &mut || {
            let file_size = backend.stable_writeback_size(inode)?;
            claimed = Self::claim_and_snapshot_tagged_locked(
                cache,
                start_index,
                end_index,
                file_size,
                required_entry,
                epoch,
                true,
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
                || backend.try_stable_writeback_size(inode),
                Self::snapshot_writeback_batch,
            );
        }
        Self::try_claim_and_snapshot_within_admission_with_stable_size(
            cache,
            &backend,
            start_index,
            end_index,
            || backend.try_stable_writeback_size(inode),
        )
    }

    /// Try to claim one batch after a backend has supplied an immediately
    /// available stable EOF.  `None` is a normal reclaimer skip: the backend
    /// may have observed an inode lock or a cold metadata cache, and PageCache
    /// must leave every candidate Dirty without issuing I/O or waiting.
    fn try_claim_and_snapshot_within_admission_with_stable_size(
        cache: &Arc<PageCache>,
        backend: &Arc<dyn PageCacheBackend>,
        start_index: usize,
        end_index: usize,
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
            claimed =
                Self::claim_and_snapshot_locked(cache, start_index, end_index, file_size, true)?;
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
    ) -> Result<WritebackNextBatchOutcome, SystemError> {
        match self.claim_next_batch_with_admission(cache, inode, start_index, end_index)? {
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
                match self.writeback_next_batch(&cache, &inode, start_index, end_index)? {
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
        let sync_inode = cache
            .inode()
            .and_then(|inode| inode.upgrade())
            .ok_or(SystemError::EIO)?;
        let _sync_retention =
            InodeRetentionGuard::new(sync_inode.clone(), InodeRetentionKind::AsyncWork)?;
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
        let sync_inode = cache
            .inode()
            .and_then(|inode| inode.upgrade())
            .ok_or(SystemError::EIO)?;
        let _sync_retention =
            InodeRetentionGuard::new(sync_inode.clone(), InodeRetentionKind::AsyncWork)?;
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

    pub fn resize(&self, len: usize) -> Result<(), SystemError> {
        let cache = self.upgrade()?;
        cache.truncate(len)
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
    ) -> Result<PageCacheWritebackDispatchOutcome, SystemError> {
        let cache = self.upgrade()?;
        let inode = cache
            .inode()
            .and_then(|inode| inode.upgrade())
            .ok_or(SystemError::EIO)?;
        match self.writeback_next_batch(&cache, &inode, start_index, end_index)? {
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
                        &cache, cursor, end_index, file_size, None, false,
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

    fn prepare_page_mkwrite_publish(
        &self,
        page_index: usize,
        page: &Arc<Page>,
        retain_transition: bool,
    ) -> Result<Option<PageCacheDirtyTransition>, SystemError> {
        let cache = self.upgrade()?;

        loop {
            let entry = {
                let inner = cache.inner.lock();
                let Some(entry) = inner.get_entry(page_index) else {
                    return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
                };
                if !Arc::ptr_eq(&entry.page, page) {
                    return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
                }
                entry
            };
            match entry.state() {
                PageState::Loading => {
                    let _ = entry.wait_ready()?;
                    continue;
                }
                PageState::Writeback => {
                    // The fault path holds AddressSpace::write(). Batch
                    // writeback publishes this state before mkclean_page()
                    // takes AddressSpace::read(), so waiting here would form
                    // mm.write -> writeback -> mm.read. The fault handler
                    // installs a wait token and retries after dropping the MM
                    // guard.
                    return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
                }
                PageState::Error => return Err(SystemError::EIO),
                PageState::UpToDate | PageState::Dirty => {}
            }

            // Retention admission is the only fallible dirty-publication
            // precondition. Do it before PG_DIRTY becomes visible; the
            // reservation is then consumed in the page->inner critical
            // section below.
            let mut reservation = cache.prepare_page_dirty()?;
            let mut page_locked = page.write();
            let mut inner = cache.inner.lock();
            let Some(current) = inner.get_entry(page_index) else {
                return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
            };
            if !Arc::ptr_eq(&current, &entry) || !Arc::ptr_eq(&current.page, page) {
                return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
            }

            match current.state() {
                PageState::Loading => {
                    drop(inner);
                    drop(page_locked);
                    let _ = entry.wait_ready()?;
                    continue;
                }
                PageState::Error => return Err(SystemError::EIO),
                PageState::UpToDate | PageState::Dirty | PageState::Writeback => {
                    // Completion uses the same page lock before sampling
                    // PG_DIRTY.  Once this flag is set there is no fallible
                    // path before the successor incarnation is published.
                    page_locked.add_flags(PageFlags::PG_DIRTY);
                    let publication = PageCache::publish_prepared_front_dirty_locked(
                        &mut inner,
                        page_index,
                        &current,
                        &mut reservation,
                    );
                    return Ok(retain_transition
                        .then(|| publication.into_transition(cache.instance_id, &current)));
                }
            }
        }
    }

    pub(crate) fn prepare_page_mkwrite_with_transition(
        &self,
        page_index: usize,
        page: &Arc<Page>,
    ) -> Result<PageCacheDirtyTransition, SystemError> {
        self.prepare_page_mkwrite_publish(page_index, page, true)?
            .ok_or(SystemError::EIO)
    }

    pub fn prepare_page_mkwrite(
        &self,
        page_index: usize,
        page: &Arc<Page>,
    ) -> Result<(), SystemError> {
        self.prepare_page_mkwrite_publish(page_index, page, false)
            .map(|_| ())
    }

    fn notify_tagged_writeback_progress(cache: &PageCache) {
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
    fn has_exact_tagged_writeback(
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
    fn has_exact_tagged_writeback_submission(
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
    fn retire_tagged_writeback_generation(
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

    fn wait_tagged_writeback_submission(
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

    fn schedule_tagged_writeback_drain(
        cache: Weak<PageCache>,
        inode: Weak<dyn IndexNode>,
        start_index: usize,
        frozen_end: usize,
        epoch: u64,
        cursor: usize,
    ) {
        Self::schedule_tagged_writeback_drain_with_permit(
            cache,
            inode,
            start_index,
            frozen_end,
            epoch,
            cursor,
            None,
        );
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
        let work_state = Mutex::new(Some(permit));
        schedule_pagecache_writeback(Work::new(move || {
            let Some(permit) = work_state.lock().take() else {
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

    fn cancel_tagged_writeback_budget_retry(cache: &PageCache, epoch: u64) {
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
                    Self::schedule_tagged_writeback_drain(
                        cache.clone(),
                        inode.clone(),
                        start_index,
                        frozen_end,
                        epoch,
                        last_index + 1,
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
                    Self::schedule_tagged_writeback_drain(
                        cache.clone(),
                        inode.clone(),
                        start_index,
                        frozen_end,
                        epoch,
                        cursor,
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
                    Self::schedule_tagged_writeback_drain(
                        cache.clone(),
                        inode.clone(),
                        start_index,
                        frozen_end,
                        epoch,
                        cursor,
                    );
                }
                PageCacheWritebackProgressOutcome::Failed(error) => {
                    Self::abandon_tagged_writeback_generation(
                        &cache_arc,
                        start_index,
                        frozen_end,
                        epoch,
                        error,
                    );
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
                        cache.record_writeback_error_with_superblock(error);
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
                target_index,
                tagged_end,
                &target_entry,
                epoch,
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
                    let still_owns_tag = {
                        let inner = cache.inner.lock();
                        inner.pages.get(&target_index).is_some_and(|current| {
                            Arc::ptr_eq(current, &target_entry)
                                && inner.dirty_pages.contains(&target_index)
                                && current.writeback_tag() == epoch
                        })
                    };
                    if still_owns_tag {
                        // A state/identity race can only make the same entry
                        // retryable again; preserve the head-first order.
                        crate::sched::sched_yield();
                        continue;
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
    /// generation under one invalidate-write exclusion boundary.
    ///
    /// The callback must follow the filesystem's normal lock order below the
    /// PageCache invalidate lock. Dispatch begins only after that exclusion is
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
        let cache = self.upgrade()?;
        let invalidate = cache.invalidate_write();
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
        let (preexisting_writeback_end, writeback_frontier) = {
            let inner = cache.inner.lock();
            (
                inner
                    .writeback_pages
                    .range(start_index..=end_index)
                    .next_back()
                    .copied(),
                inner.next_writeback_incarnation.saturating_sub(1),
            )
        };
        let (epoch, frozen_end, tagged_new) = {
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
            let dirty_end = {
                let inner = cache.inner.lock();
                inner
                    .dirty_pages
                    .range(start_index..=end_index)
                    .next_back()
                    .copied()
            };
            let Some(frozen_end) = dirty_end.into_iter().chain(preexisting_writeback_end).max()
            else {
                return Ok((
                    frozen_filesystem_state,
                    PageCacheWritebackRange {
                        cache: Arc::downgrade(&cache),
                        start_index,
                        frozen_end: None,
                        writeback_frontier,
                        epoch,
                    },
                ));
            };
            let mut tagged_new = false;
            let mut tag_cursor = start_index;
            loop {
                let last_tagged = {
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
                };
                let Some(last_tagged) = last_tagged else {
                    break;
                };
                if last_tagged >= frozen_end || last_tagged == usize::MAX {
                    break;
                }
                tag_cursor = last_tagged + 1;
                // Mirror Linux tag_pages_for_writeback(): do not monopolize
                // the page-cache index lock or CPU while tagging a very large
                // range.
                crate::sched::sched_yield();
            }
            (epoch, frozen_end, tagged_new)
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
                target_index,
                tagged_end,
                &target_entry,
                epoch,
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
                    let still_owns_tag = {
                        let inner = cache.inner.lock();
                        inner.pages.get(&target_index).is_some_and(|current| {
                            Arc::ptr_eq(current, &target_entry)
                                && inner.dirty_pages.contains(&target_index)
                                && current.writeback_tag() == epoch
                        })
                    };
                    if still_owns_tag {
                        crate::sched::sched_yield();
                        continue;
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
        let work_state = Mutex::new(Some((runner, permit, cache, inode)));
        schedule_pagecache_io(Work::new(move || {
            let Some((runner, permit, cache, inode)) = work_state.lock().take() else {
                return;
            };
            let _runner = runner;
            let _permit = permit;
            // Drain the caller's bounded scan range in file-offset order.
            // Keeping one runner per cache prevents overlapping reclaimer
            // workers, while advancing after each submitted batch avoids the
            // former one-batch-per-5-second throughput ceiling. A page dirtied
            // again behind the cursor is intentionally left for a later scan.
            let mut cursor = start_index;
            while cursor <= end_index {
                let claim = match manager
                    .try_claim_next_batch_with_admission(&cache, &inode, cursor, end_index)
                {
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

    pub fn pages_count(&self) -> Result<usize, SystemError> {
        Ok(self.upgrade()?.lock().pages_count())
    }

    pub fn supports_clean_reclaim(&self) -> bool {
        self.upgrade()
            .map(|cache| !cache.is_shmem())
            .unwrap_or(false)
    }

    /// Explicitly unlink a cache entry when it has no active user.
    ///
    /// A [`PageCachePagePin`] is also held internally while a front-end write
    /// copies into a page.  Treat that pin as a membership-liveness guarantee:
    /// a manager-initiated removal must not detach the entry between a
    /// fallible front-end reservation and its dirty-incarnation publication.
    /// Callers which receive `None` for an existing pinned page must retry
    /// after its users release their pins.
    ///
    /// This is a low-level teardown helper, not a generic reclaim operation:
    /// it deliberately does not prove that an entry is clean, not in
    /// writeback, or absent from a filesystem-private writeback protocol.
    /// Production reclaim must use the clean-only removal APIs; any future
    /// delayed-allocation teardown must first drain and resolve its own
    /// tickets before calling this helper.
    pub fn remove_page(&self, page_index: usize) -> Result<Option<Arc<Page>>, SystemError> {
        let cache = self.upgrade()?;
        let removed = {
            let mut inner = cache.lock();
            if inner
                .get_entry(page_index)
                .is_some_and(|entry| entry.active_users() == 0)
            {
                inner.remove_page(page_index)
            } else {
                None
            }
        };
        drop(cache.detach_dirty_retention_if_idle());
        Ok(removed)
    }

    /// Roll back a page newly inserted by a transactional cache operation.
    ///
    /// Identity and liveness are checked while the cache membership lock is
    /// held. If another user has acquired or mapped the page, it is no longer
    /// safe to treat it as transaction-private and the rollback leaves it in
    /// place. A successful removal also retires the page from the global page
    /// manager and reclaimer; callers must not reproduce that lifecycle logic.
    pub fn discard_created_page(
        &self,
        page_index: usize,
        expected_page: &Arc<Page>,
    ) -> Result<bool, SystemError> {
        let cache = self.upgrade()?;
        let (entry, state) = {
            let guard = cache.lock();
            let Some(entry) = guard.get_entry(page_index) else {
                return Ok(false);
            };
            if !Arc::ptr_eq(&entry.page, expected_page)
                || entry.active_users() != 0
                || guard.dirty_pages.contains(&page_index)
                || guard.writeback_pages.contains(&page_index)
            {
                return Ok(false);
            }
            let state = entry.state();
            if matches!(
                state,
                PageState::Loading | PageState::Dirty | PageState::Writeback | PageState::Error
            ) {
                return Ok(false);
            }
            (entry, state)
        };

        // Dirty publication uses the page -> cache lock order. Keep the page
        // guard through the final membership check so rollback cannot slip
        // between setting PG_DIRTY and publishing the dirty tag/state.
        let page_guard = entry.page.read();
        let page_discardable = !page_guard
            .flags()
            .intersects(PageFlags::PG_DIRTY | PageFlags::PG_WRITEBACK)
            && page_guard.map_count() == 0;
        if !page_discardable {
            return Ok(false);
        }

        let removed = {
            let mut guard = cache.lock();
            let Some(current) = guard.get_entry(page_index) else {
                return Ok(false);
            };
            if !Arc::ptr_eq(&current, &entry)
                || !Arc::ptr_eq(&current.page, expected_page)
                || current.active_users() != 0
                || current.state() != state
                || guard.dirty_pages.contains(&page_index)
                || guard.writeback_pages.contains(&page_index)
            {
                return Ok(false);
            }
            guard.remove_page(page_index)
        };
        drop(page_guard);
        let Some(page) = removed else {
            return Ok(false);
        };
        cache.discard_unlinked_page(&page);
        drop(cache.detach_dirty_retention_if_idle());
        Ok(true)
    }

    pub fn remove_clean_page_for_reclaim(
        &self,
        page_index: usize,
        expected_page: &Arc<Page>,
    ) -> Result<Option<Arc<Page>>, SystemError> {
        let cache = self.upgrade()?;
        if cache.is_shmem() {
            return Ok(None);
        }
        let entry = match cache.lock().get_entry(page_index) {
            Some(entry) => entry,
            None => return Ok(None),
        };
        if !Arc::ptr_eq(&entry.page, expected_page)
            || cache.mapping_unevictable()
            || entry.active_users() != 0
        {
            return Ok(None);
        }
        let state = entry.state();
        if matches!(
            state,
            PageState::Loading | PageState::Writeback | PageState::Error
        ) {
            return Ok(None);
        }
        let page_reclaimable = {
            let page_guard = entry.page.write();
            !page_guard.flags().intersects(
                PageFlags::PG_DIRTY | PageFlags::PG_WRITEBACK | PageFlags::PG_UNEVICTABLE,
            ) && page_guard.map_count() == 0
        };
        if !page_reclaimable {
            return Ok(None);
        }

        let mut guard = cache.lock();
        let Some(current) = guard.get_entry(page_index) else {
            return Ok(None);
        };
        if !Arc::ptr_eq(&current, &entry)
            || !Arc::ptr_eq(&current.page, expected_page)
            || cache.mapping_unevictable()
            || current.active_users() != 0
            || current.state() != state
        {
            return Ok(None);
        }
        let removed = guard.remove_page(page_index);
        Ok(removed)
    }

    pub fn writeback_page(&self, page_index: usize) -> Result<(), SystemError> {
        self.sync_data(page_index, page_index)
    }

    fn wait_writeback_entry(entry: Arc<PageEntry>) -> Result<(), SystemError> {
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

    fn writeback_incarnation_result(
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

    fn finish_writeback_entry_state(
        cache: Arc<PageCache>,
        page_index: usize,
        entry: Arc<PageEntry>,
        page: Arc<Page>,
        result: Result<(), SystemError>,
        record_error: bool,
    ) -> Result<(), SystemError> {
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
        drop(cache.detach_dirty_retention_if_idle());
        Ok(())
    }
}

impl core::fmt::Debug for PageCacheManager {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PageCacheManager").finish()
    }
}

impl PageEntry {
    fn new(page: Arc<Page>, state: PageState) -> Self {
        let instance_id = PAGE_CACHE_ENTRY_INSTANCE_ID
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                current.checked_add(1)
            })
            .expect("page-cache entry certificate identity exhausted");
        Self {
            page,
            instance_id,
            state: AtomicU8::new(state as u8),
            dirty_incarnation: AtomicU64::new(0),
            dirty_transition_kind: AtomicU8::new(0),
            writeback_tag: AtomicU64::new(0),
            writeback_incarnation: AtomicU64::new(0),
            accounting: AtomicU8::new(PageEntryAccounting::Unaccounted as u8),
            accounted_unevictable: AtomicBool::new(false),
            active_users: AtomicUsize::new(0),
            wait_queue: WaitQueue::default(),
        }
    }

    fn accounting(&self) -> PageEntryAccounting {
        match self.accounting.load(Ordering::Acquire) {
            1 => PageEntryAccounting::File,
            2 => PageEntryAccounting::Shmem,
            _ => PageEntryAccounting::Unaccounted,
        }
    }

    fn account_insert(&self, kind: PageCacheKind, mapping_unevictable: bool) {
        let accounting = match kind {
            PageCacheKind::File => PageEntryAccounting::File,
            PageCacheKind::Shmem => PageEntryAccounting::Shmem,
        };
        self.accounting
            .compare_exchange(
                PageEntryAccounting::Unaccounted as u8,
                accounting as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .expect("page-cache entry inserted more than once");
        pc_stats::inc_file_pages();
        if accounting == PageEntryAccounting::Shmem {
            pc_stats::inc_shmem_pages();
        }
        if mapping_unevictable {
            self.account_unevictable_if_needed();
        }
    }

    fn account_remove(&self) -> PageEntryAccounting {
        let accounting = match self
            .accounting
            .swap(PageEntryAccounting::Unaccounted as u8, Ordering::AcqRel)
        {
            1 => PageEntryAccounting::File,
            2 => PageEntryAccounting::Shmem,
            _ => PageEntryAccounting::Unaccounted,
        };
        if accounting == PageEntryAccounting::Unaccounted {
            return accounting;
        }

        pc_stats::dec_file_pages();
        if accounting == PageEntryAccounting::Shmem {
            pc_stats::dec_shmem_pages();
        }
        self.unaccount_unevictable_if_needed();
        match self.state() {
            PageState::Dirty => pc_stats::dec_file_dirty(),
            PageState::Writeback => {
                log::error!("detaching a page-cache entry while writeback is active");
                pc_stats::dec_file_writeback();
            }
            _ => {}
        }
        accounting
    }

    fn account_state_transition(&self, old: PageState, new: PageState) {
        if old == new || self.accounting() == PageEntryAccounting::Unaccounted {
            return;
        }
        match old {
            PageState::Dirty => pc_stats::dec_file_dirty(),
            PageState::Writeback => pc_stats::dec_file_writeback(),
            _ => {}
        }
        match new {
            PageState::Dirty => pc_stats::inc_file_dirty(),
            PageState::Writeback => pc_stats::inc_file_writeback(),
            _ => {}
        }
    }

    fn state(&self) -> PageState {
        Self::decode_state(self.state.load(Ordering::Acquire))
    }

    fn set_state(&self, state: PageState) {
        self.state.store(state as u8, Ordering::Release);
    }

    /// Construct the exact dirty lifetime observed by a front-end publisher.
    /// Callers hold the mapping `inner` lock, which serializes the state and
    /// incarnation update with Dirty/Writeback membership publication.
    fn begin_front_dirty_transition(
        &self,
        page_index: usize,
        old_state: PageState,
    ) -> PageCacheDirtyPublication {
        let kind = match old_state {
            PageState::Dirty => PageCacheDirtyTransitionKind::MergedIntoDirty,
            PageState::Writeback => PageCacheDirtyTransitionKind::RedirtiedDuringWriteback,
            PageState::Loading | PageState::UpToDate | PageState::Error => {
                PageCacheDirtyTransitionKind::NewlyDirty
            }
        };
        let dirty_incarnation = match kind {
            PageCacheDirtyTransitionKind::MergedIntoDirty => {
                let incarnation = self.dirty_incarnation.load(Ordering::Relaxed);
                assert_ne!(
                    incarnation, 0,
                    "a Dirty PageCache entry must originate from a front dirty transition"
                );
                incarnation
            }
            PageCacheDirtyTransitionKind::NewlyDirty
            | PageCacheDirtyTransitionKind::RedirtiedDuringWriteback => {
                let next = self
                    .dirty_incarnation
                    .load(Ordering::Relaxed)
                    .checked_add(1)
                    .expect("PageCache dirty incarnation exhausted");
                assert_ne!(next, 0, "PageCache dirty incarnation zero is reserved");
                self.dirty_incarnation.store(next, Ordering::Relaxed);
                self.dirty_transition_kind
                    .store(kind as u8, Ordering::Relaxed);
                next
            }
        };
        match kind {
            PageCacheDirtyTransitionKind::MergedIntoDirty => PageCacheDirtyPublication::Merged {
                page_index,
                dirty_incarnation,
            },
            PageCacheDirtyTransitionKind::NewlyDirty
            | PageCacheDirtyTransitionKind::RedirtiedDuringWriteback => {
                PageCacheDirtyPublication::Started {
                    page_index,
                    dirty_incarnation,
                    kind,
                }
            }
        }
    }

    fn dirty_incarnation(&self) -> u64 {
        self.dirty_incarnation.load(Ordering::Relaxed)
    }

    fn instance_id(&self) -> u64 {
        self.instance_id
    }

    /// Call only under this mapping's `inner` lock while the entry is known
    /// to be the candidate at `page_index` in its current dirty lifetime.
    fn current_dirty_certificate(
        &self,
        cache_instance_id: u64,
        page_index: usize,
    ) -> Result<PageCacheDirtyCertificate, SystemError> {
        let dirty_incarnation = self.dirty_incarnation.load(Ordering::Relaxed);
        let kind = PageCacheDirtyTransitionKind::from_started_u8(
            self.dirty_transition_kind.load(Ordering::Relaxed),
        )
        .ok_or(SystemError::ESTALE)?;
        if dirty_incarnation == 0 {
            return Err(SystemError::ESTALE);
        }
        Ok(PageCacheDirtyCertificate {
            cache_instance_id,
            entry_instance_id: self.instance_id,
            page_index,
            dirty_incarnation,
            kind,
        })
    }

    fn writeback_tag(&self) -> u64 {
        self.writeback_tag.load(Ordering::Acquire)
    }

    fn set_writeback_tag(&self, epoch: u64) {
        self.writeback_tag.store(epoch, Ordering::Release);
    }

    fn account_unevictable_if_needed(&self) {
        if !self.accounted_unevictable.swap(true, Ordering::AcqRel) {
            pc_stats::inc_unevictable();
        }
    }

    fn unaccount_unevictable_if_needed(&self) {
        if self.accounted_unevictable.swap(false, Ordering::AcqRel) {
            pc_stats::dec_unevictable();
        }
    }

    fn active_users(&self) -> usize {
        self.active_users.load(Ordering::Acquire)
    }

    fn wait_inactive(&self) {
        self.wait_queue.wait_until(|| {
            if self.active_users() == 0 {
                Some(())
            } else {
                None
            }
        });
    }

    fn pin(self: &Arc<Self>) -> PageEntryPin {
        self.active_users.fetch_add(1, Ordering::AcqRel);
        PageEntryPin {
            entry: self.clone(),
        }
    }

    fn wait_ready(&self) -> Result<Arc<Page>, SystemError> {
        self.wait_queue.wait_until(|| match self.state() {
            PageState::Loading => None,
            PageState::Error => Some(Err(SystemError::EIO)),
            _ => Some(Ok(self.page.clone())),
        })
    }

    fn decode_state(value: u8) -> PageState {
        match value {
            0 => PageState::Loading,
            1 => PageState::UpToDate,
            2 => PageState::Dirty,
            3 => PageState::Writeback,
            4 => PageState::Error,
            _ => PageState::Error,
        }
    }
}

struct PageEntryPin {
    entry: Arc<PageEntry>,
}

impl core::fmt::Debug for PageEntryPin {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PageEntryPin")
            .field("paddr", &self.entry.page.phys_address())
            .finish()
    }
}

impl Drop for PageEntryPin {
    fn drop(&mut self) {
        if self.entry.active_users.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.entry.wait_queue.wake_all();
        }
    }
}

#[derive(Debug)]
pub struct PageCachePagePin {
    page: Arc<Page>,
    _pin: PageEntryPin,
}

impl PageCachePagePin {
    fn new(page: Arc<Page>, pin: PageEntryPin) -> Self {
        Self { page, _pin: pin }
    }

    pub fn page(&self) -> Arc<Page> {
        self.page.clone()
    }
}

impl InnerPageCache {
    fn new(
        page_cache_ref: Weak<PageCache>,
        id: usize,
        kind: PageCacheKind,
        accounting_backend: Option<Arc<dyn PageCacheBackend>>,
    ) -> InnerPageCache {
        Self {
            id,
            pages: HashMap::new(),
            page_indices: BTreeSet::new(),
            dirty_pages: BTreeSet::new(),
            writeback_pages: BTreeSet::new(),
            next_writeback_incarnation: 1,
            next_writeback_generation: 1,
            dirty_retention: None,
            dirty_preparations: 0,
            kind,
            page_cache_ref,
            accounting_backend,
        }
    }

    pub fn get_page(&self, offset: usize) -> Option<Arc<Page>> {
        self.pages.get(&offset).map(|entry| entry.page.clone())
    }

    pub fn remove_page(&mut self, offset: usize) -> Option<Arc<Page>> {
        let entry = self.pages.remove(&offset)?;
        self.page_indices.remove(&offset);
        self.dirty_pages.remove(&offset);
        self.writeback_pages.remove(&offset);
        entry.account_remove();
        if let Some(backend) = self.accounting_backend.as_ref() {
            backend.release_page();
        }
        Some(entry.page.clone())
    }

    fn get_entry(&self, offset: usize) -> Option<Arc<PageEntry>> {
        self.pages.get(&offset).cloned()
    }

    /// Allocate the descriptor identity while the mapping state is locked.
    /// Generation zero is reserved as an invalid/default value; overflow is
    /// an admission failure rather than an ABA wrap.
    fn allocate_writeback_generation(&mut self) -> Result<u64, SystemError> {
        let generation = self.next_writeback_generation;
        self.next_writeback_generation = generation.checked_add(1).ok_or(SystemError::EOVERFLOW)?;
        Ok(generation)
    }

    fn allocate_writeback_incarnation(&mut self) -> Result<u64, SystemError> {
        let incarnation = self.next_writeback_incarnation;
        self.next_writeback_incarnation =
            incarnation.checked_add(1).ok_or(SystemError::EOVERFLOW)?;
        Ok(incarnation)
    }

    fn insert_entry(&mut self, offset: usize, entry: Arc<PageEntry>) -> Result<(), SystemError> {
        let mapping_unevictable = self
            .page_cache_ref
            .upgrade()
            .is_some_and(|cache| cache.mapping_unevictable());
        match self.pages.entry(offset) {
            Entry::Vacant(slot) => {
                if let Some(backend) = self.accounting_backend.as_ref() {
                    backend.reserve_page()?;
                }
                entry.account_insert(self.kind, mapping_unevictable);
                slot.insert(entry);
            }
            Entry::Occupied(_) => panic!("page-cache insert requires a vacant slot"),
        }
        self.page_indices.insert(offset);
        Ok(())
    }

    fn is_page_ready(&self, offset: usize) -> bool {
        self.pages
            .get(&offset)
            .map(|entry| entry.state().is_ready())
            .unwrap_or(false)
    }

    pub fn pages_count(&self) -> usize {
        return self.pages.len();
    }
}

impl Drop for InnerPageCache {
    fn drop(&mut self) {
        // log::debug!("page cache drop");
        let page_addrs = self
            .pages
            .values()
            .map(|entry| entry.page.phys_address())
            .collect::<Vec<_>>();
        let mut page_manager = page_manager_lock();
        for entry in self.pages.values() {
            entry.account_remove();
            if let Some(backend) = self.accounting_backend.as_ref() {
                backend.release_page();
            }
            page_manager.remove_page(&entry.page.phys_address());
        }
        drop(page_manager);

        let mut reclaimer = page_reclaimer_lock();
        for paddr in page_addrs {
            reclaimer.remove_page(&paddr);
        }
    }
}

impl PageCache {
    // Lock order: page_cache -> page_manager -> page_reclaimer.
    // Avoid holding page_cache lock while acquiring page_manager when possible.
    pub fn new(
        inode: Option<Weak<dyn IndexNode>>,
        backend: Option<Arc<dyn PageCacheBackend>>,
    ) -> Arc<PageCache> {
        Self::new_with_kind(inode, backend, PageCacheKind::File)
    }

    pub fn new_shmem(
        inode: Option<Weak<dyn IndexNode>>,
        backend: Option<Arc<dyn PageCacheBackend>>,
    ) -> Arc<PageCache> {
        Self::new_with_kind(inode, backend, PageCacheKind::Shmem)
    }

    fn new_with_kind(
        inode: Option<Weak<dyn IndexNode>>,
        backend: Option<Arc<dyn PageCacheBackend>>,
        kind: PageCacheKind,
    ) -> Arc<PageCache> {
        let id = PAGE_CACHE_ID.fetch_add(1, Ordering::SeqCst);
        let instance_id = PAGE_CACHE_INSTANCE_ID
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                current.checked_add(1)
            })
            .expect("page-cache certificate identity exhausted");
        // Quota accounting is a shmem concern.  Regular file backends use the
        // same trait for I/O, but its accounting hooks are the default no-ops;
        // retaining them here would add an unnecessary indirect call to every
        // file-page insertion and removal while the cache lock is held.
        let accounting_backend = if kind == PageCacheKind::Shmem {
            backend.clone()
        } else {
            None
        };
        let cache = Arc::new_cyclic(|weak| Self {
            id,
            instance_id,
            inner: Mutex::new(InnerPageCache::new(
                weak.clone(),
                id,
                kind,
                accounting_backend,
            )),
            inode: {
                let v: Lazy<Weak<dyn IndexNode>> = Lazy::new();
                if let Some(inode) = inode {
                    v.init(inode);
                }
                v
            },
            backend: {
                let v: Lazy<Arc<dyn PageCacheBackend>> = Lazy::new();
                if let Some(backend) = backend {
                    v.init(backend);
                }
                v
            },
            i_mmap_rwsem: RwSem::new(()),
            invalidate_lock: RwSem::new(()),
            file_vma_seq: AtomicU64::new(0),
            file_vmas: SpinLock::new(FileVmaIndex::default()),
            writeback_error: ErrSeq::new(),
            unevictable: AtomicBool::new(false),
            kind,
            reclassify_lock: Mutex::new(()),
            tagged_writeback_lock: Mutex::new(()),
            tagged_writeback_progress: AtomicU64::new(0),
            tagged_writeback_wait: WaitQueue::default(),
            tagged_writeback_submissions: Mutex::new(Vec::new()),
            tagged_writeback_budget_retries: Mutex::new(HashMap::new()),
            writeback_protocol: AtomicU8::new(PageCacheWritebackProtocolState::Unset as u8),
            reclaimer_writeback_active: AtomicBool::new(false),
            manager: PageCacheManager::new(weak.clone()),
        });
        register_page_cache(&cache);
        cache
    }

    pub fn sample_writeback_error(&self) -> ErrSeqValue {
        self.writeback_error.sample()
    }

    pub fn check_writeback_error_since(&self, since: ErrSeqValue) -> Option<SystemError> {
        self.writeback_error.check(since)
    }

    pub fn check_and_advance_writeback_error(
        &self,
        since: &mut ErrSeqValue,
    ) -> Option<SystemError> {
        self.writeback_error.check_and_advance(since)
    }

    fn record_writeback_error(&self, error: SystemError) {
        self.writeback_error.set(error);
    }

    /// Record a writeback error in the page cache mapping and, while it is
    /// still alive, its mounted superblock, matching Linux mapping_set_error()
    /// semantics without assuming that a weak filesystem owner survived an
    /// asynchronous writeback completion.
    pub fn record_writeback_error_with_superblock(&self, error: SystemError) {
        self.record_writeback_error(error.clone());
        if let Some(inode) = self.inode().and_then(|w| w.upgrade()) {
            if let Some(fs) = inode.try_fs() {
                record_writeback_error_for_fs(&fs, error);
            }
        }
    }

    /// # 获取页缓存的ID
    #[inline]
    #[allow(unused)]
    pub fn id(&self) -> usize {
        self.id
    }

    /// Fast check for dirty pages (no full dirty-set traversal, just emptiness test).
    pub fn has_dirty_pages(&self) -> bool {
        !self.inner.lock().dirty_pages.is_empty()
    }

    pub fn inode(&self) -> Option<Weak<dyn IndexNode>> {
        self.inode.try_get().cloned()
    }

    pub fn set_inode(&self, inode: Weak<dyn IndexNode>) -> Result<(), SystemError> {
        if self.inode.initialized() {
            return Err(SystemError::EINVAL);
        }
        self.inode.init(inode);
        Ok(())
    }

    pub fn set_backend(&self, backend: Arc<dyn PageCacheBackend>) -> Result<(), SystemError> {
        if self.backend.initialized() {
            return Err(SystemError::EINVAL);
        }
        self.backend.init(backend);
        Ok(())
    }

    pub fn backend(&self) -> Option<Arc<dyn PageCacheBackend>> {
        self.backend.try_get().cloned()
    }

    pub fn lock(&self) -> MutexGuard<'_, InnerPageCache> {
        self.inner.lock()
    }

    pub fn manager(&self) -> &PageCacheManager {
        &self.manager
    }

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

    pub fn drop_clean_pages(&self) -> usize {
        if self.is_shmem() {
            return 0;
        }
        self.evict_clean_pages_for_invalidate(None)
    }

    fn clean_evict_indices(&self, range: Option<(usize, usize)>) -> Vec<usize> {
        let guard = self.inner.lock();
        match range {
            Some((start, end)) if start > end => Vec::new(),
            Some((start, end)) => guard.page_indices.range(start..=end).copied().collect(),
            None => guard.page_indices.iter().copied().collect(),
        }
    }

    fn remove_clean_page_candidate(
        &self,
        page_index: usize,
        wait_loading: bool,
    ) -> Option<Arc<Page>> {
        loop {
            let entry = {
                let guard = self.inner.lock();
                guard.get_entry(page_index)
            }?;

            match entry.state() {
                PageState::Loading => {
                    if !wait_loading {
                        return None;
                    }
                    let _ = entry.wait_ready();
                    continue;
                }
                PageState::UpToDate | PageState::Error => {}
                PageState::Dirty | PageState::Writeback => return None,
            }

            if self.mapping_unevictable() || entry.active_users() != 0 {
                return None;
            }

            let page_reclaimable = {
                let page_guard = entry.page.read();
                !page_guard.flags().intersects(
                    PageFlags::PG_DIRTY | PageFlags::PG_WRITEBACK | PageFlags::PG_UNEVICTABLE,
                ) && page_guard.map_count() == 0
            };
            if !page_reclaimable {
                return None;
            }

            let mut guard = self.inner.lock();
            let current = guard.get_entry(page_index)?;
            if !Arc::ptr_eq(&current, &entry) {
                continue;
            }
            if self.mapping_unevictable()
                || current.active_users() != 0
                || !matches!(current.state(), PageState::UpToDate | PageState::Error)
            {
                return None;
            }
            return guard.remove_page(page_index);
        }
    }

    fn evict_clean_pages_for_invalidate(&self, range: Option<(usize, usize)>) -> usize {
        let mut evicted = 0;
        for page_index in self.clean_evict_indices(range) {
            if let Some(page) = self.remove_clean_page_candidate(page_index, true) {
                let paddr = page.phys_address();
                page_manager_lock().remove_page(&paddr);
                let _ = page_reclaimer_lock().remove_page(&paddr);
                evicted += 1;
            }
        }
        evicted
    }

    /// Mark this page cache as unevictable (or revert). When enabled, newly created
    /// pages will carry PG_UNEVICTABLE to keep the reclaimer from reclaiming them.
    pub fn set_unevictable(&self, unevictable: bool) -> bool {
        self.unevictable.swap(unevictable, Ordering::Relaxed)
    }

    pub fn mapping_unevictable(&self) -> bool {
        self.unevictable.load(Ordering::Relaxed)
    }

    fn is_shmem(&self) -> bool {
        self.kind == PageCacheKind::Shmem
    }

    fn page_flags(&self) -> PageFlags {
        if self.mapping_unevictable() {
            PageFlags::PG_LRU | PageFlags::PG_UNEVICTABLE
        } else {
            PageFlags::PG_LRU
        }
    }

    pub fn reclassify_unevictable_pages(&self, old_mapping_unevictable: bool) {
        const RECLASSIFY_BATCH: usize = 64;

        let _reclassify_guard = self.reclassify_lock.lock();
        let mapping_unevictable = self.mapping_unevictable();
        if old_mapping_unevictable == mapping_unevictable {
            return;
        }

        let mut next_index = 0usize;
        loop {
            let entries = {
                let guard = self.inner.lock();
                guard
                    .page_indices
                    .range(next_index..)
                    .take(RECLASSIFY_BATCH)
                    .filter_map(|index| {
                        guard.pages.get(index).cloned().map(|entry| (*index, entry))
                    })
                    .collect::<Vec<_>>()
            };
            if entries.is_empty() {
                break;
            }
            let last_index = entries[entries.len() - 1].0;
            if last_index == usize::MAX {
                next_index = usize::MAX;
            } else {
                next_index = last_index + 1;
            }

            for (index, entry) in entries {
                let page = &entry.page;
                if mapping_unevictable {
                    if !self.mapping_unevictable() {
                        return;
                    }
                    let mut page_guard = page.write();
                    let guard = self.inner.lock();
                    let Some(current) = guard.pages.get(&index) else {
                        continue;
                    };
                    if !Arc::ptr_eq(current, &entry) {
                        continue;
                    }
                    if !self.mapping_unevictable() {
                        continue;
                    }

                    let was_unevictable = page_guard.flags().contains(PageFlags::PG_UNEVICTABLE);
                    if !was_unevictable {
                        page_guard.add_flags(PageFlags::PG_UNEVICTABLE);
                    }
                    let paddr = page.phys_address();
                    drop(page_guard);
                    entry.account_unevictable_if_needed();
                    drop(guard);
                    if !was_unevictable {
                        let _ = page_reclaimer_lock().remove_page(&paddr);
                    }
                } else {
                    let mut page_guard = page.write();
                    let guard = self.inner.lock();
                    let Some(current) = guard.pages.get(&index) else {
                        continue;
                    };
                    if !Arc::ptr_eq(current, &entry) || self.mapping_unevictable() {
                        continue;
                    }

                    let keep_unevictable = page_guard.has_unevictable_source();
                    let was_unevictable = page_guard.flags().contains(PageFlags::PG_UNEVICTABLE);
                    entry.unaccount_unevictable_if_needed();
                    if !keep_unevictable && was_unevictable {
                        page_guard.remove_flags(PageFlags::PG_UNEVICTABLE);
                        let paddr = page.phys_address();
                        let should_reclaim =
                            !self.is_shmem() && page_guard.flags().contains(PageFlags::PG_LRU);
                        drop(page_guard);
                        drop(guard);
                        if should_reclaim {
                            page_reclaimer_lock().insert_page(paddr, page);
                        }
                    }
                }
            }
            if next_index == usize::MAX {
                break;
            }
        }
    }

    fn reconcile_entry_unevictable_for_insert(&self, entry: &PageEntry) {
        let mapping_unevictable = self.mapping_unevictable();
        let paddr = entry.page.phys_address();
        if mapping_unevictable {
            let mut page_guard = entry.page.write();
            let was_unevictable = page_guard.flags().contains(PageFlags::PG_UNEVICTABLE);
            if !was_unevictable {
                page_guard.add_flags(PageFlags::PG_UNEVICTABLE);
            }
            drop(page_guard);
            if !was_unevictable {
                let _ = page_reclaimer_lock().remove_page(&paddr);
            }
            return;
        }

        entry.unaccount_unevictable_if_needed();
        let mut page_guard = entry.page.write();
        let was_unevictable = page_guard.flags().contains(PageFlags::PG_UNEVICTABLE);
        if was_unevictable && !page_guard.has_unevictable_source() {
            page_guard.remove_flags(PageFlags::PG_UNEVICTABLE);
            let should_reclaim = page_guard.flags().contains(PageFlags::PG_LRU);
            drop(page_guard);
            if should_reclaim {
                page_reclaimer_lock().insert_page(paddr, &entry.page);
            }
        }
    }

    fn allocate_page(
        &self,
        page_cache_ref: Weak<PageCache>,
        page_index: usize,
    ) -> Result<Arc<Page>, SystemError> {
        let mut page_manager_guard = page_manager_lock();
        page_manager_guard.create_one_page(
            PageType::File(FileMapInfo {
                page_cache: page_cache_ref,
                index: page_index,
            }),
            self.page_flags(),
            &mut LockedFrameAllocator,
        )
    }

    fn populate_page_from_backend(
        &self,
        page_index: usize,
        page: &Arc<Page>,
    ) -> Result<(), SystemError> {
        let backend = self.backend();
        if let Some(backend) = backend {
            let waiter = backend.read_page_async(page_index, page);
            let read_len = waiter.wait()?;
            if read_len < MMArch::PAGE_SIZE {
                let mut page_guard = page.write();
                let dst = unsafe { page_guard.as_slice_mut() };
                dst[read_len..MMArch::PAGE_SIZE].fill(0);
            }
            page.write().add_flags(PageFlags::PG_UPTODATE);
            return Ok(());
        }

        let inode = self
            .inode()
            .and_then(|inode| inode.upgrade())
            .ok_or(SystemError::EIO)?;
        let mut page_guard = page.write();
        let dst = unsafe { page_guard.as_slice_mut() };
        inode.read_sync(page_index * MMArch::PAGE_SIZE, dst)?;
        page_guard.add_flags(PageFlags::PG_UPTODATE);
        Ok(())
    }

    fn populate_page_zero(&self, page: &Arc<Page>) -> Result<(), SystemError> {
        let mut page_guard = page.write();
        unsafe {
            page_guard.as_slice_mut().fill(0);
        }
        page_guard.add_flags(PageFlags::PG_UPTODATE);
        Ok(())
    }

    fn get_or_create_entry(
        &self,
        page_index: usize,
        populate_backend: bool,
    ) -> Result<Arc<PageEntry>, SystemError> {
        self.get_or_create_entry_with_status(page_index, populate_backend)
            .map(|(entry, _created)| entry)
    }

    fn get_or_create_entry_with_status(
        &self,
        page_index: usize,
        populate_backend: bool,
    ) -> Result<(Arc<PageEntry>, bool), SystemError> {
        let mut page_cache_ref = None;
        let mut existing_entry = None;
        {
            let guard = self.inner.lock();
            if let Some(entry) = guard.get_entry(page_index) {
                existing_entry = Some(entry);
            } else {
                page_cache_ref = Some(guard.page_cache_ref.clone());
            }
        }

        if let Some(entry) = existing_entry {
            let state = entry.state();
            if state.is_ready() {
                return Ok((entry, false));
            }
            if state == PageState::Error {
                return Err(SystemError::EIO);
            }
            let _ = entry.wait_ready()?;
            return Ok((entry, false));
        }

        let (entry, need_populate) = {
            let guard = self.inner.lock();
            if let Some(entry) = guard.get_entry(page_index) {
                (entry, false)
            } else {
                drop(guard);
                let page = self.allocate_page(
                    page_cache_ref.expect("page_cache_ref should exist"),
                    page_index,
                )?;
                let mut guard = self.inner.lock();
                if let Some(entry) = guard.get_entry(page_index) {
                    self.discard_unlinked_page(&page);
                    (entry, false)
                } else {
                    let entry = Arc::new(PageEntry::new(page, PageState::Loading));
                    if let Err(error) = guard.insert_entry(page_index, entry.clone()) {
                        drop(guard);
                        self.discard_unlinked_page(&entry.page);
                        return Err(error);
                    }
                    (entry, true)
                }
            }
        };

        if !need_populate {
            let state = entry.state();
            if state.is_ready() {
                return Ok((entry, false));
            }
            if state == PageState::Error {
                return Err(SystemError::EIO);
            }
            let _ = entry.wait_ready()?;
            return Ok((entry, false));
        }
        self.reconcile_entry_unevictable_for_insert(&entry);

        let populate_result = if populate_backend {
            self.populate_page_from_backend(page_index, &entry.page)
        } else {
            self.populate_page_zero(&entry.page)
        };

        match populate_result {
            Ok(()) => {
                entry.set_state(PageState::UpToDate);
                entry.wait_queue.wake_all();
                Ok((entry, true))
            }
            Err(e) => {
                entry.set_state(PageState::Error);
                entry.wait_queue.wake_all();
                self.remove_failed_entry(page_index, &entry);
                Err(e)
            }
        }
    }

    fn get_or_create_entry_pinned(
        &self,
        page_index: usize,
        populate_backend: bool,
    ) -> Result<(Arc<PageEntry>, PageEntryPin), SystemError> {
        loop {
            let entry = self.get_or_create_entry(page_index, populate_backend)?;
            let guard = self.inner.lock();
            let Some(current) = guard.get_entry(page_index) else {
                continue;
            };
            if !Arc::ptr_eq(&current, &entry) || !entry.state().is_ready() {
                continue;
            }
            let pin = entry.pin();
            return Ok((entry, pin));
        }
    }

    fn remove_failed_entry(&self, page_index: usize, entry: &Arc<PageEntry>) {
        let mut guard = self.inner.lock();
        if let Some(current) = guard.get_entry(page_index) {
            if Arc::ptr_eq(&current, entry) {
                guard.remove_page(page_index);
            }
        }
        self.discard_unlinked_page(&entry.page);
    }

    fn discard_error_entry(&self, page_index: usize) {
        let removed = {
            let mut guard = self.inner.lock();
            let Some(entry) = guard.get_entry(page_index) else {
                return;
            };
            if entry.state() != PageState::Error {
                return;
            }
            guard.remove_page(page_index)
        };

        if let Some(page) = removed {
            self.discard_unlinked_page(&page);
        }
    }

    fn discard_error_entry_if_same(&self, page_index: usize, expected: &Arc<PageEntry>) -> bool {
        let removed = {
            let mut guard = self.inner.lock();
            let Some(entry) = guard.get_entry(page_index) else {
                return false;
            };
            if !Arc::ptr_eq(&entry, expected) || entry.state() != PageState::Error {
                return false;
            }
            guard.remove_page(page_index)
        };
        if let Some(page) = removed {
            self.discard_unlinked_page(&page);
            true
        } else {
            false
        }
    }

    fn discard_unlinked_page(&self, page: &Arc<Page>) {
        let paddr = page.phys_address();
        let can_remove_from_manager = {
            let mut page_guard = page.write();
            page_guard.clear_unlinked_file_mapping_unevictable();
            page_guard.can_deallocate()
        };
        if can_remove_from_manager {
            page_manager_lock().remove_page(&paddr);
        }
        let _ = page_reclaimer_lock().remove_page(&paddr);
    }

    fn start_async_read(&self, page_index: usize) -> Result<(), SystemError> {
        let mut existing_entry = None;
        let mut page_cache_ref = None;
        {
            let guard = self.inner.lock();
            if let Some(entry) = guard.get_entry(page_index) {
                existing_entry = Some(entry);
            } else {
                page_cache_ref = Some(guard.page_cache_ref.clone());
            }
        }

        if let Some(entry) = existing_entry {
            let state = entry.state();
            if matches!(
                state,
                PageState::Loading | PageState::Writeback | PageState::Error
            ) {
                return Ok(());
            }
            return Ok(());
        }

        let entry = {
            let guard = self.inner.lock();
            if guard.get_entry(page_index).is_some() {
                return Ok(());
            }
            drop(guard);
            let page = self.allocate_page(
                page_cache_ref.expect("page_cache_ref should exist"),
                page_index,
            )?;
            let mut guard = self.inner.lock();
            if guard.get_entry(page_index).is_some() {
                self.discard_unlinked_page(&page);
                return Ok(());
            }
            let entry = Arc::new(PageEntry::new(page, PageState::Loading));
            if let Err(error) = guard.insert_entry(page_index, entry.clone()) {
                drop(guard);
                self.discard_unlinked_page(&entry.page);
                return Err(error);
            }
            entry
        };
        self.reconcile_entry_unevictable_for_insert(&entry);

        let backend = self.backend();
        let inode = self.inode();
        let entry_clone = entry.clone();
        let page = entry.page.clone();

        let work = Work::new(move || {
            let read_len = if let Some(backend) = backend.as_ref() {
                backend.read_page_async(page_index, &page).wait()
            } else if let Some(inode) = inode.as_ref().and_then(|inode| inode.upgrade()) {
                let mut guard = page.write();
                let dst = unsafe { guard.as_slice_mut() };
                inode.read_sync(page_index * MMArch::PAGE_SIZE, dst)
            } else {
                Err(SystemError::EIO)
            };

            match read_len {
                Ok(len) => {
                    if len < MMArch::PAGE_SIZE {
                        let mut guard = page.write();
                        let dst = unsafe { guard.as_slice_mut() };
                        dst[len..MMArch::PAGE_SIZE].fill(0);
                    }
                    page.write().add_flags(PageFlags::PG_UPTODATE);
                    entry_clone.set_state(PageState::UpToDate);
                }
                Err(_) => {
                    page.write().add_flags(PageFlags::PG_ERROR);
                    entry_clone.set_state(PageState::Error);
                }
            }
            entry_clone.wait_queue.wake_all();
        });
        schedule_work(work);
        Ok(())
    }

    pub fn is_page_ready(&self, page_index: usize) -> bool {
        self.inner.lock().is_page_ready(page_index)
    }

    /// Test an entire half-open page range while holding the cache lock once.
    pub fn is_range_ready(&self, start_page_index: usize, end_page_index: usize) -> bool {
        if start_page_index >= end_page_index {
            return true;
        }
        let inner = self.inner.lock();
        (start_page_index..end_page_index).all(|index| inner.is_page_ready(index))
    }

    /// Wait for an entry that actually conflicts with a DMA reservation range.
    ///
    /// The conflicting entry may disappear between `reserve_read_dma()` returning
    /// `EEXIST` and this lookup.  In that case the caller should simply retry
    /// discovery instead of creating a new entry for an index that never
    /// conflicted.
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

    pub fn get_ready_page(&self, page_index: usize) -> Option<Arc<Page>> {
        let guard = self.inner.lock();
        guard
            .get_entry(page_index)
            .filter(|entry| entry.state().is_ready())
            .map(|entry| entry.page.clone())
    }

    pub fn get_ready_page_pinned(&self, page_index: usize) -> Option<PageCachePagePin> {
        let guard = self.inner.lock();
        let entry = guard.get_entry(page_index)?;
        if !entry.state().is_ready() {
            return None;
        }
        let pin = entry.pin();
        Some(PageCachePagePin::new(entry.page.clone(), pin))
    }

    pub fn get_or_create_page_for_read(&self, page_index: usize) -> Result<Arc<Page>, SystemError> {
        Ok(self.get_or_create_entry(page_index, true)?.page.clone())
    }

    pub fn get_or_create_page_for_read_pinned(
        &self,
        page_index: usize,
    ) -> Result<PageCachePagePin, SystemError> {
        self.get_or_create_page_pinned(page_index, true)
    }

    pub fn get_or_create_page_with<F>(
        &self,
        page_index: usize,
        fill: F,
    ) -> Result<Arc<Page>, SystemError>
    where
        F: FnOnce(usize, &mut [u8]) -> Result<usize, SystemError>,
    {
        let mut page_cache_ref = None;
        let mut existing_entry = None;
        {
            let guard = self.inner.lock();
            if let Some(entry) = guard.get_entry(page_index) {
                existing_entry = Some(entry);
            } else {
                page_cache_ref = Some(guard.page_cache_ref.clone());
            }
        }

        if let Some(entry) = existing_entry {
            let state = entry.state();
            if state.is_ready() {
                return Ok(entry.page.clone());
            }
            if state == PageState::Error {
                return Err(SystemError::EIO);
            }
            let page = entry.wait_ready()?;
            return Ok(page);
        }

        let (entry, need_populate) = {
            let guard = self.inner.lock();
            if let Some(entry) = guard.get_entry(page_index) {
                (entry, false)
            } else {
                drop(guard);
                let page = self.allocate_page(
                    page_cache_ref.expect("page_cache_ref should exist"),
                    page_index,
                )?;
                let mut guard = self.inner.lock();
                if let Some(entry) = guard.get_entry(page_index) {
                    self.discard_unlinked_page(&page);
                    (entry, false)
                } else {
                    let entry = Arc::new(PageEntry::new(page, PageState::Loading));
                    if let Err(error) = guard.insert_entry(page_index, entry.clone()) {
                        drop(guard);
                        self.discard_unlinked_page(&entry.page);
                        return Err(error);
                    }
                    (entry, true)
                }
            }
        };

        if !need_populate {
            let state = entry.state();
            if state.is_ready() {
                return Ok(entry.page.clone());
            }
            if state == PageState::Error {
                return Err(SystemError::EIO);
            }
            return entry.wait_ready();
        }
        self.reconcile_entry_unevictable_for_insert(&entry);

        let populate_result = {
            let mut tmp = vec![0; MMArch::PAGE_SIZE];
            match fill(page_index, &mut tmp) {
                Ok(read_len) if read_len <= MMArch::PAGE_SIZE => {
                    let mut page_guard = entry.page.write();
                    let dst = unsafe { page_guard.as_slice_mut() };
                    dst.copy_from_slice(&tmp);
                    page_guard.add_flags(PageFlags::PG_UPTODATE);
                    Ok(())
                }
                Ok(_) => Err(SystemError::EIO),
                Err(e) => Err(e),
            }
        };

        match populate_result {
            Ok(()) => {
                entry.set_state(PageState::UpToDate);
                entry.wait_queue.wake_all();
                Ok(entry.page.clone())
            }
            Err(e) => {
                entry.set_state(PageState::Error);
                entry.wait_queue.wake_all();
                self.remove_failed_entry(page_index, &entry);
                Err(e)
            }
        }
    }

    /// Populate a page for a write, replacing only a pre-existing Error entry.
    /// Errors produced by this call's own fill operation are returned without
    /// retry so persistent backend failures cannot turn into an infinite loop.
    fn get_or_create_page_for_write_with<F>(
        &self,
        page_index: usize,
        fill: F,
    ) -> Result<Arc<Page>, SystemError>
    where
        F: FnOnce(usize, &mut [u8]) -> Result<usize, SystemError>,
    {
        let mut fill = Some(fill);
        loop {
            let mut page_cache_ref = None;
            let existing_entry = {
                let guard = self.inner.lock();
                match guard.get_entry(page_index) {
                    Some(entry) => Some(entry),
                    None => {
                        page_cache_ref = Some(guard.page_cache_ref.clone());
                        None
                    }
                }
            };

            if let Some(entry) = existing_entry {
                match entry.state() {
                    state if state.is_ready() => return Ok(entry.page.clone()),
                    PageState::Error => {
                        self.discard_error_entry_if_same(page_index, &entry);
                        continue;
                    }
                    PageState::Loading | PageState::Writeback => match entry.wait_ready() {
                        Ok(page) => return Ok(page),
                        Err(_e) if entry.state() == PageState::Error => {
                            self.discard_error_entry_if_same(page_index, &entry);
                            continue;
                        }
                        Err(e) => return Err(e),
                    },
                    PageState::UpToDate | PageState::Dirty => unreachable!(),
                }
            }

            let page = self.allocate_page(
                page_cache_ref.expect("page_cache_ref should exist"),
                page_index,
            )?;
            let entry = Arc::new(PageEntry::new(page, PageState::Loading));
            let inserted = {
                let mut guard = self.inner.lock();
                if guard.get_entry(page_index).is_some() {
                    false
                } else {
                    if let Err(error) = guard.insert_entry(page_index, entry.clone()) {
                        drop(guard);
                        self.discard_unlinked_page(&entry.page);
                        return Err(error);
                    }
                    true
                }
            };
            if !inserted {
                self.discard_unlinked_page(&entry.page);
                continue;
            }
            self.reconcile_entry_unevictable_for_insert(&entry);

            let populate_result = {
                let mut tmp = vec![0; MMArch::PAGE_SIZE];
                match fill.take().expect("write page fill consumed once")(page_index, &mut tmp) {
                    Ok(read_len) if read_len <= MMArch::PAGE_SIZE => {
                        let mut page_guard = entry.page.write();
                        let dst = unsafe { page_guard.as_slice_mut() };
                        dst.copy_from_slice(&tmp);
                        page_guard.add_flags(PageFlags::PG_UPTODATE);
                        Ok(())
                    }
                    Ok(_) => Err(SystemError::EIO),
                    Err(e) => Err(e),
                }
            };

            match populate_result {
                Ok(()) => {
                    entry.set_state(PageState::UpToDate);
                    entry.wait_queue.wake_all();
                    return Ok(entry.page.clone());
                }
                Err(e) => {
                    entry.set_state(PageState::Error);
                    entry.wait_queue.wake_all();
                    self.remove_failed_entry(page_index, &entry);
                    return Err(e);
                }
            }
        }
    }

    pub fn get_or_create_page_zero(&self, page_index: usize) -> Result<Arc<Page>, SystemError> {
        Ok(self.get_or_create_entry(page_index, false)?.page.clone())
    }

    pub fn get_or_create_page_zero_pinned(
        &self,
        page_index: usize,
    ) -> Result<PageCachePagePin, SystemError> {
        self.get_or_create_page_pinned(page_index, false)
    }

    pub fn get_or_create_page_zero_pinned_with_status(
        &self,
        page_index: usize,
    ) -> Result<(PageCachePagePin, bool), SystemError> {
        loop {
            let (entry, created) = self.get_or_create_entry_with_status(page_index, false)?;
            let guard = self.inner.lock();
            let Some(current) = guard.get_entry(page_index) else {
                continue;
            };
            if !Arc::ptr_eq(&current, &entry) || !entry.state().is_ready() {
                continue;
            }
            let pin = entry.pin();
            return Ok((PageCachePagePin::new(entry.page.clone(), pin), created));
        }
    }

    fn get_or_create_page_pinned(
        &self,
        page_index: usize,
        populate_backend: bool,
    ) -> Result<PageCachePagePin, SystemError> {
        loop {
            let entry = self.get_or_create_entry(page_index, populate_backend)?;
            let guard = self.inner.lock();
            let Some(current) = guard.get_entry(page_index) else {
                continue;
            };
            if !Arc::ptr_eq(&current, &entry) || !entry.state().is_ready() {
                continue;
            }
            let pin = entry.pin();
            return Ok(PageCachePagePin::new(entry.page.clone(), pin));
        }
    }

    fn ensure_dirty_retention_locked(&self, inner: &mut InnerPageCache) -> Result<(), SystemError> {
        // Shmem pages have no asynchronous backing-store writeback. The inode
        // already owns the mapping, so retaining the inode from the mapping's
        // dirty state would form a permanent inode -> page cache -> inode cycle.
        // In particular, an unlinked tmpfs inode would never release its page
        // reservations. Ordinary file mappings still need this guard while
        // dirty/writeback work can outlive the caller.
        if self.is_shmem() {
            return Ok(());
        }
        if inner.dirty_retention.is_some() {
            return Ok(());
        }
        let inode = self
            .inode()
            .and_then(|inode| inode.upgrade())
            .ok_or(SystemError::EIO)?;
        inner.dirty_retention = Some(InodeRetentionGuard::new(
            inode,
            InodeRetentionKind::AsyncWork,
        )?);
        Ok(())
    }

    /// Establish dirty backing ownership before callers expose modified data.
    pub fn prepare_page_dirty(&self) -> Result<PageDirtyReservation, SystemError> {
        let mut inner = self.inner.lock();
        self.ensure_dirty_retention_locked(&mut inner)?;
        inner.dirty_preparations = inner
            .dirty_preparations
            .checked_add(1)
            .expect("page-cache dirty preparation overflow");
        Ok(PageDirtyReservation {
            cache: self.manager.owner.clone(),
            active: true,
        })
    }

    fn cancel_page_dirty_reservation(&self) {
        let mut inner = self.inner.lock();
        assert!(inner.dirty_preparations != 0);
        inner.dirty_preparations -= 1;
        drop(inner);
        drop(self.detach_dirty_retention_if_idle());
    }

    fn detach_dirty_retention_if_idle(&self) -> Option<InodeRetentionGuard> {
        let mut inner = self.inner.lock();
        if inner.dirty_preparations == 0
            && inner.dirty_pages.is_empty()
            && inner.writeback_pages.is_empty()
        {
            inner.dirty_retention.take()
        } else {
            None
        }
    }

    /// Publish a front-end dirty transition while `inner` serializes entry
    /// identity, state and membership.  The caller must still hold the exact
    /// page's write lock after setting `PG_DIRTY`: completion also takes that
    /// lock before sampling the flag, so it cannot fold this write into an
    /// older writeback incarnation between flagging and publication.
    fn publish_front_dirty_locked(
        dirty_pages: &mut BTreeSet<usize>,
        page_index: usize,
        entry: &Arc<PageEntry>,
    ) -> PageCacheDirtyPublication {
        let old_state = entry.state();
        let transition = entry.begin_front_dirty_transition(page_index, old_state);
        dirty_pages.insert(page_index);
        if old_state != PageState::Writeback {
            entry.account_state_transition(old_state, PageState::Dirty);
            entry.set_state(PageState::Dirty);
        }
        transition
    }

    /// Consume an already-admitted dirty reservation while `inner` is held
    /// and publish the corresponding front dirty transition. Callers use this
    /// only after all identity/state checks have completed, so setting
    /// `PG_DIRTY` immediately beforehand cannot be followed by a fallible
    /// operation that leaves it orphaned from `dirty_pages`.
    fn publish_prepared_front_dirty_locked(
        inner: &mut InnerPageCache,
        page_index: usize,
        entry: &Arc<PageEntry>,
        reservation: &mut PageDirtyReservation,
    ) -> PageCacheDirtyPublication {
        assert!(reservation.active);
        assert!(inner.dirty_preparations != 0);
        inner.dirty_preparations -= 1;
        reservation.active = false;
        Self::publish_front_dirty_locked(&mut inner.dirty_pages, page_index, entry)
    }

    fn validate_page_locked_entry(
        entry: &Arc<PageEntry>,
        page_locked: &InnerPage,
    ) -> Result<(), SystemError> {
        if entry.page.phys_address() != page_locked.phys_address() {
            return Err(SystemError::ESTALE);
        }
        Ok(())
    }

    /// Mark a page dirty while the caller retains that exact page's write
    /// lock, then return the exact front-end incarnation when it started one.
    /// A future filesystem ticket bridge must retain `Started` rather than
    /// reconstructing identity from a later writeback descriptor.
    pub(crate) fn mark_page_dirty_page_locked_with_transition(
        &self,
        page_index: usize,
        page_locked: &InnerPage,
    ) -> Result<Option<PageCacheDirtyTransition>, SystemError> {
        let mut guard = self.inner.lock();
        self.ensure_dirty_retention_locked(&mut guard)?;
        let Some(entry) = guard.get_entry(page_index) else {
            drop(guard);
            drop(self.detach_dirty_retention_if_idle());
            return Ok(None);
        };
        Self::validate_page_locked_entry(&entry, page_locked)?;
        let publication =
            Self::publish_front_dirty_locked(&mut guard.dirty_pages, page_index, &entry);
        Ok(Some(publication.into_transition(self.instance_id, &entry)))
    }

    /// Lightweight front dirty publish for the current eager callers.  It
    /// intentionally does not construct the future ticket capability.
    pub(crate) fn mark_page_dirty_page_locked(
        &self,
        page_index: usize,
        page_locked: &InnerPage,
    ) -> Result<(), SystemError> {
        let mut guard = self.inner.lock();
        self.ensure_dirty_retention_locked(&mut guard)?;
        let Some(entry) = guard.get_entry(page_index) else {
            drop(guard);
            drop(self.detach_dirty_retention_if_idle());
            return Ok(());
        };
        Self::validate_page_locked_entry(&entry, page_locked)?;
        let _ = Self::publish_front_dirty_locked(&mut guard.dirty_pages, page_index, &entry);
        Ok(())
    }

    pub(crate) fn mark_page_dirty_prepared_page_locked_with_transition(
        &self,
        page_index: usize,
        reservation: &mut PageDirtyReservation,
        page_locked: &InnerPage,
    ) -> Result<Option<PageCacheDirtyTransition>, SystemError> {
        let mut guard = self.inner.lock();
        let Some(entry) = guard.get_entry(page_index) else {
            assert!(reservation.active);
            assert!(guard.dirty_preparations != 0);
            guard.dirty_preparations -= 1;
            reservation.active = false;
            drop(guard);
            drop(self.detach_dirty_retention_if_idle());
            return Ok(None);
        };
        Self::validate_page_locked_entry(&entry, page_locked)?;
        let publication =
            Self::publish_prepared_front_dirty_locked(&mut guard, page_index, &entry, reservation);
        Ok(Some(publication.into_transition(self.instance_id, &entry)))
    }

    pub(crate) fn mark_page_dirty_prepared_page_locked(
        &self,
        page_index: usize,
        reservation: &mut PageDirtyReservation,
        page_locked: &InnerPage,
    ) -> Result<(), SystemError> {
        let mut guard = self.inner.lock();
        let Some(entry) = guard.get_entry(page_index) else {
            assert!(reservation.active);
            assert!(guard.dirty_preparations != 0);
            guard.dirty_preparations -= 1;
            reservation.active = false;
            drop(guard);
            drop(self.detach_dirty_retention_if_idle());
            return Ok(());
        };
        Self::validate_page_locked_entry(&entry, page_locked)?;
        let _ =
            Self::publish_prepared_front_dirty_locked(&mut guard, page_index, &entry, reservation);
        Ok(())
    }

    /// Revalidate an opaque front dirty event before a future filesystem
    /// bridge binds a ticket.  A removed/recreated entry, page-index mismatch
    /// or successor dirty incarnation is stale; callers must never infer a
    /// new ticket from the current page state in that case.
    pub(crate) fn validate_dirty_incarnation(
        &self,
        transition: &PageCacheDirtyIncarnation,
    ) -> Result<(), SystemError> {
        let inner = self.inner.lock();
        let current = inner
            .get_entry(transition.page_index())
            .ok_or(SystemError::ESTALE)?;
        if !Arc::ptr_eq(&current, &transition.entry)
            || !matches!(current.state(), PageState::Dirty | PageState::Writeback)
        {
            return Err(SystemError::ESTALE);
        }
        let current_certificate =
            current.current_dirty_certificate(self.instance_id, transition.page_index())?;
        if current_certificate != transition.certificate() {
            return Err(SystemError::ESTALE);
        }
        Ok(())
    }

    /// Claim writeback only while the page locked by the caller is still the
    /// entry attached at this index. A reclaimer snapshot may outlive mapping
    /// removal, so index alone is not a sufficient identity.
    pub fn try_mark_page_writeback(
        &self,
        page_index: usize,
        expected_paddr: crate::mm::PhysAddr,
    ) -> bool {
        if self.writeback_protocol.load(Ordering::Acquire)
            == PageCacheWritebackProtocolState::Token as u8
            || self.backend().as_ref().is_some_and(|backend| {
                backend.writeback_submission_protocol() == PageCacheWritebackProtocol::Token
            })
        {
            // This legacy one-page path neither creates a descriptor nor
            // calls bind_writeback_submission().  Once a mapping declares
            // ordered Token writeback (even before its first successful
            // bind), admitting it here would bypass the ticket's ordering and
            // cancellation contract.  Leave it Dirty for the normal batch
            // claim path instead.
            return false;
        }
        let mut guard = self.inner.lock();
        if let Some(entry) = guard.get_entry(page_index) {
            if entry.page.phys_address() != expected_paddr
                || matches!(
                    entry.state(),
                    PageState::Loading | PageState::Writeback | PageState::Error
                )
            {
                return false;
            }
            let old_state = entry.state();
            entry.account_state_transition(old_state, PageState::Writeback);
            entry.set_state(PageState::Writeback);
            guard.dirty_pages.remove(&page_index);
            guard.writeback_pages.insert(page_index);
            return true;
        }
        false
    }

    pub fn mark_page_uptodate(&self, page_index: usize) {
        let mut guard = self.inner.lock();
        if let Some(entry) = guard.get_entry(page_index) {
            let old_state = entry.state();
            entry.account_state_transition(old_state, PageState::UpToDate);
            entry.set_state(PageState::UpToDate);
            guard.dirty_pages.remove(&page_index);
            guard.writeback_pages.remove(&page_index);
        }
        drop(guard);
        drop(self.detach_dirty_retention_if_idle());
    }

    pub fn mark_page_error(&self, page_index: usize, error: SystemError) {
        self.record_writeback_error_with_superblock(error);
        let mut guard = self.inner.lock();
        if let Some(entry) = guard.get_entry(page_index) {
            let old_state = entry.state();
            entry.account_state_transition(old_state, PageState::Dirty);
            guard.dirty_pages.insert(page_index);
            guard.writeback_pages.remove(&page_index);
            entry.set_state(PageState::Dirty);
            entry.wait_queue.wake_all();
        }
    }

    /// Insert a pre-allocated page into page cache and mark it ready.
    /// This is for special in-kernel users (e.g. perf ring buffers).
    pub fn insert_ready_page(&self, page_index: usize, page: Arc<Page>) -> Result<(), SystemError> {
        let entry = Arc::new(PageEntry::new(page, PageState::UpToDate));
        let _reclassify_guard = self.reclassify_lock.lock();
        {
            let guard = self.inner.lock();
            if guard.get_entry(page_index).is_some() {
                return Err(SystemError::EEXIST);
            }
        }
        self.reconcile_entry_unevictable_for_insert(&entry);
        let mut guard = self.inner.lock();
        if guard.get_entry(page_index).is_some() {
            drop(guard);
            self.discard_unlinked_page(&entry.page);
            return Err(SystemError::EEXIST);
        }
        match guard.insert_entry(page_index, entry.clone()) {
            Ok(()) => Ok(()),
            Err(error) => {
                drop(guard);
                self.discard_unlinked_page(&entry.page);
                Err(error)
            }
        }
    }

    pub fn read_pages(&self, start_page_index: usize, page_num: usize) -> Result<(), SystemError> {
        for i in 0..page_num {
            self.start_async_read(start_page_index + i)?;
        }
        Ok(())
    }

    /// 两阶段读取：持锁收集拷贝项，解锁后拷贝到目标缓冲区，避免用户缺页导致自锁
    pub fn read(&self, offset: usize, buf: &mut [u8]) -> Result<usize, SystemError> {
        let inode = self
            .inode()
            .and_then(|inode| inode.upgrade())
            .ok_or(SystemError::EIO)?;
        let file_size = inode.metadata()?.size;

        let len = if offset < file_size as usize {
            core::cmp::min(file_size as usize, offset + buf.len()) - offset
        } else {
            0
        };

        if len == 0 {
            return Ok(0);
        }

        let start_page_index = offset >> MMArch::PAGE_SHIFT;
        let end_page_index = (offset + len - 1) >> MMArch::PAGE_SHIFT;

        let mut copies: Vec<CopyItem> = Vec::new();
        let mut ret = 0usize;

        for page_index in start_page_index..=end_page_index {
            let page_start = page_index * MMArch::PAGE_SIZE;
            let page_end = page_start + MMArch::PAGE_SIZE;

            let read_start = core::cmp::max(offset, page_start);
            let read_end = core::cmp::min(offset + len, page_end);
            let page_read_len = read_end.saturating_sub(read_start);
            if page_read_len == 0 {
                continue;
            }

            let (entry, pin) = self.get_or_create_entry_pinned(page_index, true)?;
            copies.push(CopyItem {
                entry,
                _pin: pin,
                page_index,
                page_offset: read_start - page_start,
                sub_len: page_read_len,
            });
            ret += page_read_len;
        }

        let mut dst_offset = 0;
        for item in copies {
            // 先prefault，避免在持锁后触发缺页
            let byte = volatile_read!(buf[dst_offset]);
            volatile_write!(buf[dst_offset], byte);
            let page_guard = item.entry.page.read();
            unsafe {
                buf[dst_offset..dst_offset + item.sub_len].copy_from_slice(
                    &page_guard.as_slice()[item.page_offset..item.page_offset + item.sub_len],
                );
            }
            dst_offset += item.sub_len;
        }

        Ok(ret)
    }

    pub fn write(&self, offset: usize, buf: &[u8]) -> Result<usize, SystemError> {
        let (copies, ret) = self.prepare_write_copies(offset, buf.len())?;
        let mut dirty_reservation = if ret != 0 {
            Some(self.prepare_page_dirty()?)
        } else {
            None
        };
        let mut src_offset = 0;
        for item in copies {
            // Prefault before taking the page lock.
            let _ = volatile_read!(buf[src_offset]);
            let mut page_guard = item.entry.page.write();
            unsafe {
                page_guard.as_slice_mut()[item.page_offset..item.page_offset + item.sub_len]
                    .copy_from_slice(&buf[src_offset..src_offset + item.sub_len]);
            }
            page_guard.add_flags(PageFlags::PG_DIRTY);
            src_offset += item.sub_len;
            if let Some(mut reservation) = dirty_reservation.take() {
                self.mark_page_dirty_prepared_page_locked(
                    item.page_index,
                    &mut reservation,
                    &page_guard,
                )?;
            } else {
                self.mark_page_dirty_page_locked(item.page_index, &page_guard)?;
            }
        }
        Ok(ret)
    }

    /// Publish one full, page-aligned dirty page and hand its exact front
    /// dirty transition to a caller while both the page write lock and this
    /// cache's `inner` lock are still held.
    ///
    /// This is deliberately narrower than [`Self::write`]. It is the future
    /// delayed-allocation bridge for the initial `BLOCK_SIZE == PAGE_SIZE`
    /// prototype: front-end admission creates a fallible, Drop-rollback
    /// reservation *before* entering PageCache, then `on_published` records
    /// the non-forgeable dirty-incarnation capability without a window in
    /// which writeback can claim the page first.
    ///
    /// `on_published` is intentionally infallible and must not allocate,
    /// block, acquire any lock, re-enter PageCache, or drop an object whose
    /// destructor can do any of those things. At that point bytes and
    /// PG_DIRTY have become visible, so returning an error would require
    /// rolling back user data. Its caller must prepare all queue storage
    /// before this call, and use RAII to cancel that preparation if a
    /// PageCache-local fallible step returns an error. The callback may only
    /// consume the prepared state. A `Merged` transition must consume or
    /// release a duplicate external reservation without creating another
    /// ticket. Keeping this contract crate-private prevents ordinary eager
    /// callers from accidentally depending on a partial delayed-allocation
    /// protocol.
    pub(crate) fn write_single_full_page_with_transition<G>(
        &self,
        offset: usize,
        buf: &[u8],
        on_published: G,
    ) -> Result<usize, SystemError>
    where
        G: FnOnce(PageCacheDirtyTransition),
    {
        if offset & (MMArch::PAGE_SIZE - 1) != 0 || buf.len() != MMArch::PAGE_SIZE {
            return Err(SystemError::EINVAL);
        }
        offset
            .checked_add(MMArch::PAGE_SIZE)
            .ok_or(SystemError::EFBIG)?;

        let (mut copies, ret) = self.prepare_write_copies(offset, buf.len())?;
        debug_assert_eq!(ret, MMArch::PAGE_SIZE);
        let item = copies.pop().ok_or(SystemError::EIO)?;
        debug_assert!(copies.is_empty());
        debug_assert_eq!(item.page_offset, 0);
        debug_assert_eq!(item.sub_len, MMArch::PAGE_SIZE);

        // Prefault before either the page lock or external front-admission
        // state becomes live. The copy below is therefore the only operation
        // between the caller's successful preparation and dirty publication.
        let _ = volatile_read!(buf[0]);
        let _ = volatile_read!(buf[MMArch::PAGE_SIZE - 1]);

        // Retention is the one PageCache-local fallible precondition. If a
        // later local validation fails, its Drop leaves no hidden dirty
        // lifetime behind and the caller's own RAII reservation can roll back.
        let mut dirty_reservation = self.prepare_page_dirty()?;
        let mut page_guard = item.entry.page.write();

        // The active pin keeps the page lifetime valid. Holding `inner` from
        // attachment validation through callback handoff prevents removal or
        // Dirty -> Writeback from observing the newly copied bytes before the
        // caller records the matching ticket.
        let mut inner = self.inner.lock();
        let current = inner
            .get_entry(item.page_index)
            .ok_or(SystemError::ESTALE)?;
        if !Arc::ptr_eq(&current, &item.entry) || !Arc::ptr_eq(&current.page, &item.entry.page) {
            return Err(SystemError::ESTALE);
        }

        unsafe {
            page_guard.as_slice_mut().copy_from_slice(buf);
        }
        page_guard.add_flags(PageFlags::PG_DIRTY);
        let publication = Self::publish_prepared_front_dirty_locked(
            &mut inner,
            item.page_index,
            &current,
            &mut dirty_reservation,
        );
        // A `Started` transition temporarily clones `current.entry`. The
        // callback may drop it immediately, but this cannot be the final
        // reference: `current`, `item.entry`, and this mapping's `inner`
        // membership all remain live until the callback returns. Front-end
        // handoff code must retain only its Copy certificate; an Arc is not a
        // membership pin and must not escape as a delayed-map ticket.
        on_published(publication.into_transition(self.instance_id, &current));
        Ok(ret)
    }

    /// Publish one segment of exactly one page after validating the caller's
    /// expected dirty transition before copying any user byte.
    ///
    /// `Merge` is tied to the exact existing dirty certificate. A stale
    /// queue-tail observation therefore returns `EAGAIN` without changing
    /// page contents, dirty accounting, or an external reservation.
    pub(crate) fn write_single_page_segment_with_transition<G>(
        &self,
        offset: usize,
        buf: &[u8],
        expected: PageCacheExpectedDirtyTransition,
        on_published: G,
    ) -> Result<usize, SystemError>
    where
        G: FnOnce(PageCacheDirtyTransition),
    {
        if buf.is_empty() {
            return Ok(0);
        }
        let page_index = offset >> MMArch::PAGE_SHIFT;
        let page_offset = offset & (MMArch::PAGE_SIZE - 1);
        if page_offset
            .checked_add(buf.len())
            .is_none_or(|end| end > MMArch::PAGE_SIZE)
        {
            return Err(SystemError::EINVAL);
        }

        let (mut copies, ret) = self.prepare_write_copies(offset, buf.len())?;
        let item = copies.pop().ok_or(SystemError::EIO)?;
        if !copies.is_empty()
            || ret != buf.len()
            || item.page_index != page_index
            || item.page_offset != page_offset
            || item.sub_len != buf.len()
        {
            return Err(SystemError::EIO);
        }

        let _ = volatile_read!(buf[0]);
        let _ = volatile_read!(buf[buf.len() - 1]);
        let mut dirty_reservation = match expected {
            PageCacheExpectedDirtyTransition::Start => Some(self.prepare_page_dirty()?),
            PageCacheExpectedDirtyTransition::Merge(_) => None,
        };
        let mut page_guard = item.entry.page.write();
        let mut inner = self.inner.lock();
        let current = inner
            .get_entry(item.page_index)
            .ok_or(SystemError::ESTALE)?;
        if !Arc::ptr_eq(&current, &item.entry) || !Arc::ptr_eq(&current.page, &item.entry.page) {
            return Err(SystemError::ESTALE);
        }

        match expected {
            PageCacheExpectedDirtyTransition::Start => {
                if inner.dirty_pages.contains(&item.page_index)
                    || matches!(current.state(), PageState::Dirty | PageState::Writeback)
                {
                    return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
                }
            }
            PageCacheExpectedDirtyTransition::Merge(certificate) => {
                if current.state() != PageState::Dirty
                    || !inner.dirty_pages.contains(&item.page_index)
                    || current.current_dirty_certificate(self.instance_id, item.page_index)?
                        != certificate
                {
                    return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
                }
            }
        }

        unsafe {
            page_guard.as_slice_mut()[item.page_offset..item.page_offset + item.sub_len]
                .copy_from_slice(buf);
        }
        page_guard.add_flags(PageFlags::PG_DIRTY);
        let publication = match dirty_reservation.as_mut() {
            Some(reservation) => Self::publish_prepared_front_dirty_locked(
                &mut inner,
                item.page_index,
                &current,
                reservation,
            ),
            None => {
                Self::publish_front_dirty_locked(&mut inner.dirty_pages, item.page_index, &current)
            }
        };
        on_published(publication.into_transition(self.instance_id, &current));
        Ok(ret)
    }

    fn prepare_write_copies(
        &self,
        offset: usize,
        len: usize,
    ) -> Result<(Vec<CopyItem>, usize), SystemError> {
        if len == 0 {
            return Ok((Vec::new(), 0));
        }

        let start_page_index = offset >> MMArch::PAGE_SHIFT;
        let end_page_index = (offset + len - 1) >> MMArch::PAGE_SHIFT;
        let mut copies: Vec<CopyItem> = Vec::new();
        let mut ret = 0usize;

        for page_index in start_page_index..=end_page_index {
            let page_start = page_index * MMArch::PAGE_SIZE;
            let page_end = page_start + MMArch::PAGE_SIZE;
            let write_start = core::cmp::max(offset, page_start);
            let write_end = core::cmp::min(offset + len, page_end);
            let page_write_len = write_end.saturating_sub(write_start);
            if page_write_len == 0 {
                continue;
            }

            let full_page_overwrite =
                write_start == page_start && page_write_len == MMArch::PAGE_SIZE;
            let populate_backend = !self.is_shmem() && !full_page_overwrite;
            self.discard_error_entry(page_index);
            let (entry, pin) = self.get_or_create_entry_pinned(page_index, populate_backend)?;
            copies.push(CopyItem {
                entry,
                _pin: pin,
                page_index,
                page_offset: write_start - page_start,
                sub_len: page_write_len,
            });
            ret += page_write_len;
        }

        Ok((copies, ret))
    }

    /// Two-phase write: prepare and pin every destination page before
    /// committing metadata or exposing dirty data.
    ///
    /// `before_dirty` runs after all fallible page preparation has completed
    /// and while every destination page is write-locked, but before any caller
    /// data is copied or dirty state becomes visible. The locks remain held
    /// through the copy and dirty transition, making the metadata, data, and
    /// dirty state externally visible as one ordered commit.
    pub(crate) fn write_with_before_dirty<F>(
        &self,
        offset: usize,
        buf: &[u8],
        before_dirty: F,
    ) -> Result<usize, SystemError>
    where
        F: FnOnce(usize) -> Result<(), SystemError>,
    {
        let (copies, ret) = self.prepare_write_copies(offset, buf.len())?;
        if ret == 0 {
            return Ok(0);
        }

        let mut src_offset = 0;
        for item in &copies {
            // Prefault each source segment before the metadata commit so the
            // remaining page-locked copy path cannot introduce a new failure
            // point after the filesystem publishes the write.
            let _ = volatile_read!(buf[src_offset]);
            src_offset += item.sub_len;
        }

        // Lock in ascending page-index order (the same order as `copies`) so
        // readers and writeback cannot observe metadata for the new EOF until
        // all copied bytes and PG_DIRTY transitions are ready to be exposed.
        let mut page_guards: Vec<_> = copies.iter().map(|item| item.entry.page.write()).collect();

        let mut dirty_reservation = self.prepare_page_dirty()?;
        before_dirty(ret)?;

        src_offset = 0;
        for (item, page_guard) in copies.iter().zip(page_guards.iter_mut()) {
            unsafe {
                page_guard.as_slice_mut()[item.page_offset..item.page_offset + item.sub_len]
                    .copy_from_slice(&buf[src_offset..src_offset + item.sub_len]);
            }
            page_guard.add_flags(PageFlags::PG_DIRTY);
            src_offset += item.sub_len;
        }
        for (index, (item, page_guard)) in copies.iter().zip(page_guards.iter()).enumerate() {
            if index == 0 {
                self.mark_page_dirty_prepared_page_locked(
                    item.page_index,
                    &mut dirty_reservation,
                    page_guard,
                )?;
            } else {
                self.mark_page_dirty_page_locked(item.page_index, page_guard)?;
            }
        }

        Ok(ret)
    }
}
