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

mod mapping;
mod read_dma;
mod selftest;
mod writeback;
use mapping::FileVmaIndex;
pub(crate) use mapping::PageCacheFaultInvalidateRead;
pub use mapping::UnmapMappingMode;
pub use read_dma::PageCacheReadDmaReservation;
pub(crate) use selftest::{run_accounting_debug_selftest, run_completion_domain_debug_selftest};
pub(crate) use writeback::{
    async_writeback_progress_snapshot, wait_for_async_writeback_progress,
    PageCacheWritebackDispatchOutcome,
};
use writeback::{
    run_async_writeback_budget_retry_selftest, ClaimedWritebackBatch, TaggedWritebackBudgetRetry,
    TaggedWritebackSubmission, WritebackClaimOutcome, WritebackSubmitOutcome,
    PAGECACHE_WRITEBACK_WQS,
};
pub use writeback::{
    AsyncPageCacheBackend, PageCacheBackend, PageCacheWritebackAdmissionOrder,
    PageCacheWritebackBindResult, PageCacheWritebackCancellationContext,
    PageCacheWritebackDescriptor, PageCacheWritebackProgress, PageCacheWritebackProgressOutcome,
    PageCacheWritebackSnapshotPhase, PageCacheWritebackSubmission, PageCacheWritebackSubmitResult,
};

static PAGE_CACHE_ID: AtomicUsize = AtomicUsize::new(0);
/// Certificate identities are separate from diagnostic/cache-table ids and
/// are never permitted to wrap: a delayed-allocation ticket may outlive a
/// removed entry or cache until its drain transaction resolves it.
static PAGE_CACHE_INSTANCE_ID: AtomicU64 = AtomicU64::new(1);
static PAGE_CACHE_ENTRY_INSTANCE_ID: AtomicU64 = AtomicU64::new(1);

const PAGECACHE_IO_WORKERS: usize = 4;
static PAGECACHE_IO_RR: AtomicUsize = AtomicUsize::new(0);

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

lazy_static! {
    static ref PAGECACHE_IO_WQS: Vec<Arc<WorkQueue>> = {
        let mut wqs = Vec::new();
        for i in 0..PAGECACHE_IO_WORKERS {
            wqs.push(WorkQueue::new(&format!("pagecache-io-{i}")));
        }
        wqs
    };
    static ref PAGECACHE_REGISTRY: SpinLock<Vec<Weak<PageCache>>> = SpinLock::new(Vec::new());
}

pub(crate) fn schedule_pagecache_io(work: Arc<Work>) {
    let idx = PAGECACHE_IO_RR.fetch_add(1, Ordering::Relaxed) % PAGECACHE_IO_WQS.len();
    PAGECACHE_IO_WQS[idx].enqueue(work);
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
