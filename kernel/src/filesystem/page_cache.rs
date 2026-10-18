use core::{
    mem::{self, ManuallyDrop},
    sync::atomic::{AtomicBool, AtomicU64, AtomicU8, AtomicUsize, Ordering},
};

use alloc::{
    boxed::Box,
    collections::{BTreeMap, VecDeque},
    sync::{Arc, Weak},
    vec::Vec,
};
use hashbrown::{hash_map::Entry, HashMap};
use system_error::SystemError;

use super::vfs::{
    inode_lifecycle::{InodeRetentionGuard, InodeRetentionKind},
    mount::SuperBlockState,
    FilePrivateData, FileSystem, IndexNode, WritebackControl,
};
use crate::exception::workqueue::{schedule_work, Work, WorkQueue};
use crate::libs::errseq::{ErrSeq, ErrSeqValue};
use crate::libs::lazy_init::Lazy;
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
use crate::{
    arch::MMArch,
    libs::mutex::Mutex,
    mm::{
        mmu_gather::MmuGather,
        page::{
            detach_pages_from_manager_batched, detach_pages_from_reclaimer_batched,
            page_manager_lock, page_reclaimer_lock, InnerPage, ManagedPageBatch, Page, PageFlags,
        },
        ucontext::AddressSpace,
        MemoryManagementArch,
    },
};
use crate::{libs::align::page_align_up, mm::page::PageType};
use lazy_static::lazy_static;

mod mapping;
mod ordered_index;
mod read_batch;
mod read_dma;
mod selftest;
mod writeback;
use crate::cgroup::memory::{
    accounting::{KernelByteKind, MemoryAccount, MemoryKind},
    object::{ObjectAccount, ObjectCharge},
    owned::OwnedBox,
};
use crate::mm::allocation::MemoryAllocationError;
use mapping::FileVmaIndex;
pub(crate) use mapping::PageCacheFaultInvalidateRead;
pub(crate) use mapping::PageCacheRangeResult;
pub(crate) use mapping::PageCacheReclaimMapOutcome;
pub use mapping::UnmapMappingMode;
use ordered_index::{CacheIndex, Node as CacheIndexNode};
pub use read_batch::{PageCacheReadBatchCompletion, PageCacheReadBatchRequest};
pub use read_dma::PageCacheReadDmaReservation;
pub(crate) use selftest::{run_accounting_debug_selftest, run_completion_domain_debug_selftest};
pub(crate) use writeback::{
    async_writeback_progress_snapshot, schedule_pagecache_writeback,
    wait_for_async_writeback_progress, PageCacheDomainWriteback, PageCacheWritebackDispatchOutcome,
    DOMAIN_WRITEBACK_WINDOW,
};
use writeback::{
    run_async_writeback_budget_retry_selftest, run_submitted_writeback_selftest,
    ClaimedWritebackBatch, TaggedWritebackBudgetRetry, TaggedWritebackIncarnationRetry,
    TaggedWritebackSubmission, WritebackBatchRange, WritebackClaimOutcome, WritebackExecutor,
    WritebackSubmitOutcome,
};
pub use writeback::{
    AsyncPageCacheBackend, PageCacheBackend, PageCacheWritebackAdmissionOrder,
    PageCacheWritebackBindResult, PageCacheWritebackCancellationContext,
    PageCacheWritebackCompletion, PageCacheWritebackDescriptor, PageCacheWritebackProgress,
    PageCacheWritebackProgressOutcome, PageCacheWritebackSnapshotPhase,
    PageCacheWritebackSubmission, PageCacheWritebackSubmitResult,
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
    static ref PAGECACHE_REGISTRY: SpinLock<PageCacheRegistry> =
        SpinLock::new(PageCacheRegistry::new());
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DomainAdmission {
    Active,
    Closed,
}

struct PageCacheWritebackDomainBinding {
    owner: Weak<dyn FileSystem>,
    superblock: Weak<SuperBlockState>,
}

struct PageCacheWritebackDomainState {
    binding: Option<PageCacheWritebackDomainBinding>,
    admission: DomainAdmission,
    io_in_flight: usize,
    registrations_in_flight: usize,
}

/// Stable ownership and shutdown domain for file-backed page caches.
///
/// The registry is deliberately separate from `state`: registry compaction
/// may scan and allocate, while I/O admission and completion require a short,
/// non-allocating spin critical section.
pub struct PageCacheWritebackDomain {
    registry: Mutex<Vec<Weak<dyn PageCacheDomainMember>>>,
    state: SpinLock<PageCacheWritebackDomainState>,
    drained: Completion,
    writeback_executor: Mutex<Option<Arc<WritebackExecutor>>>,
}

pub(crate) trait PageCacheDomainMember: Send + Sync {
    fn has_dirty_or_writeback_work(&self) -> bool;
    fn start_sync_for_domain(&self) -> Result<PageCacheDomainWriteback, SystemError>;
    fn retire_for_domain(&self) -> Result<(), SystemError>;
}

impl core::fmt::Debug for PageCacheWritebackDomain {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let state = self.state.lock_irqsave();
        f.debug_struct("PageCacheWritebackDomain")
            .field("admission", &state.admission)
            .field("io_in_flight", &state.io_in_flight)
            .field("registrations_in_flight", &state.registrations_in_flight)
            .finish()
    }
}

impl PageCacheWritebackDomain {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            registry: Mutex::new(Vec::new()),
            state: SpinLock::new(PageCacheWritebackDomainState {
                binding: None,
                admission: DomainAdmission::Active,
                io_in_flight: 0,
                registrations_in_flight: 0,
            }),
            drained: Completion::new(),
            writeback_executor: Mutex::new(None),
        })
    }

    fn writeback_executor(&self) -> Result<Arc<WritebackExecutor>, SystemError> {
        // Sleeping initialization lock: never hold domain state or page locks
        // across kernel-thread creation. A failed creation leaves no executor.
        let mut slot = self.writeback_executor.lock();
        if slot.is_none() {
            *slot = Some(WritebackExecutor::try_new()?);
        }
        Ok(slot.as_ref().expect("initialized executor").clone())
    }

    pub fn bind(
        &self,
        owner: &Arc<dyn FileSystem>,
        superblock: &Arc<SuperBlockState>,
    ) -> Result<(), SystemError> {
        let mut state = self.state.lock_irqsave();
        match state.binding.as_ref() {
            None if state.admission == DomainAdmission::Active => {
                state.binding = Some(PageCacheWritebackDomainBinding {
                    owner: Arc::downgrade(owner),
                    superblock: Arc::downgrade(superblock),
                });
                Ok(())
            }
            Some(binding)
                if Weak::ptr_eq(&binding.owner, &Arc::downgrade(owner))
                    && Weak::ptr_eq(&binding.superblock, &Arc::downgrade(superblock)) =>
            {
                Ok(())
            }
            _ => Err(SystemError::EINVAL),
        }
    }

    /// Reuse the existing domain/SB identity for canonical orphan accounting.
    /// The short domain lock is released before callers touch inode state.
    pub(crate) fn bound_superblock(&self) -> Option<Arc<SuperBlockState>> {
        self.state
            .lock_irqsave()
            .binding
            .as_ref()?
            .superblock
            .upgrade()
    }

    fn finish_registration(&self) {
        let complete = {
            let mut state = self.state.lock_irqsave();
            state.registrations_in_flight -= 1;
            state.admission == DomainAdmission::Closed
                && state.io_in_flight == 0
                && state.registrations_in_flight == 0
        };
        if complete {
            self.drained.complete();
        }
    }

    fn register(&self, cache: &Arc<PageCache>) -> Result<(), SystemError> {
        {
            let mut state = self.state.lock_irqsave();
            if state.admission != DomainAdmission::Active {
                return Err(SystemError::ESTALE);
            }
            state.registrations_in_flight = state
                .registrations_in_flight
                .checked_add(1)
                .expect("page-cache domain registration count overflow");
        }

        {
            let mut registry = self.registry.lock();
            if registry.len() == registry.capacity() {
                registry.retain(|entry| entry.strong_count() != 0);
                let slack = core::cmp::max(8, registry.len() / 4);
                registry.reserve(slack);
            }
            let member: Arc<dyn PageCacheDomainMember> = cache.clone();
            registry.push(Arc::downgrade(&member));
        }
        self.finish_registration();
        Ok(())
    }

    pub(crate) fn snapshot(&self) -> Vec<Arc<dyn PageCacheDomainMember>> {
        let mut registry = self.registry.lock();
        let mut result = Vec::new();
        registry.retain(|entry| {
            if let Some(cache) = entry.upgrade() {
                result.push(cache);
                true
            } else {
                false
            }
        });
        result
    }

    pub fn record_writeback_error(&self, error: SystemError) {
        let superblock = self
            .state
            .lock_irqsave()
            .binding
            .as_ref()
            .and_then(|binding| binding.superblock.upgrade());
        if let Some(superblock) = superblock {
            superblock.record_wb_error(error);
        }
    }

    pub fn close_background_admission(&self) {
        let complete = {
            let mut state = self.state.lock_irqsave();
            state.admission = DomainAdmission::Closed;
            state.io_in_flight == 0 && state.registrations_in_flight == 0
        };
        if complete {
            self.drained.complete();
        }
    }

    pub fn wait_drained(&self) -> Result<(), SystemError> {
        self.drained.wait_for_completion().map(|_| ())
    }

    fn try_acquire_io_classified(
        self: &Arc<Self>,
    ) -> Result<PageCacheDomainIoPermit, PageCacheDomainIoAdmissionError> {
        let owner = {
            let mut state = self.state.lock_irqsave();
            if state.admission != DomainAdmission::Active {
                return Err(PageCacheDomainIoAdmissionError::Closed);
            }
            let owner = state
                .binding
                .as_ref()
                .and_then(|binding| binding.owner.upgrade())
                .ok_or(PageCacheDomainIoAdmissionError::Unavailable(
                    SystemError::ESTALE,
                ))?;
            state.io_in_flight = state
                .io_in_flight
                .checked_add(1)
                .expect("page-cache domain I/O count overflow");
            owner
        };
        Ok(PageCacheDomainIoPermit {
            domain: self.clone(),
            _owner: owner,
        })
    }

    pub fn try_acquire_io(self: &Arc<Self>) -> Result<PageCacheDomainIoPermit, SystemError> {
        self.try_acquire_io_classified()
            .map_err(|error| match error {
                PageCacheDomainIoAdmissionError::Closed => SystemError::ESTALE,
                PageCacheDomainIoAdmissionError::Unavailable(error) => error,
            })
    }
}

#[derive(Debug)]
pub struct PageCacheDomainIoPermit {
    domain: Arc<PageCacheWritebackDomain>,
    _owner: Arc<dyn FileSystem>,
}

pub(crate) enum PageCacheDomainIoAdmissionError {
    Closed,
    Unavailable(SystemError),
}

impl Drop for PageCacheDomainIoPermit {
    fn drop(&mut self) {
        let complete = {
            let mut state = self.domain.state.lock_irqsave();
            state.io_in_flight -= 1;
            state.admission == DomainAdmission::Closed
                && state.io_in_flight == 0
                && state.registrations_in_flight == 0
        };
        if complete {
            self.domain.drained.complete();
        }
    }
}

impl PageCacheDomainIoPermit {
    /// Run admitted producer continuations in the same filesystem dependency
    /// domain as writeback. In particular, final shutdown may wait on this
    /// permit from SYSTEM_WQ, so these producers must never use that queue.
    /// Do not reacquire admission: a closed domain must drain its accepted work.
    pub(crate) fn writeback_executor(&self) -> Result<Arc<WritebackExecutor>, SystemError> {
        self.domain.writeback_executor()
    }

    pub(crate) fn belongs_to(&self, domain: &Arc<PageCacheWritebackDomain>) -> bool {
        Arc::ptr_eq(&self.domain, domain)
    }

    /// Derive a child permit from an already admitted I/O lineage.
    ///
    /// Closing a domain rejects new producers, but it must not revoke work
    /// accepted before the close linearization point.  A child permit keeps
    /// the same owner alive and extends the in-flight count without
    /// consulting the public admission state.
    pub(crate) fn derive(&self) -> Self {
        {
            let mut state = self.domain.state.lock_irqsave();
            debug_assert!(state.io_in_flight != 0);
            state.io_in_flight = state
                .io_in_flight
                .checked_add(1)
                .expect("page-cache domain I/O count overflow");
        }
        Self {
            domain: self.domain.clone(),
            _owner: self._owner.clone(),
        }
    }
}

pub(crate) fn schedule_pagecache_io(work: Arc<Work>) {
    let idx = PAGECACHE_IO_RR.fetch_add(1, Ordering::Relaxed) % PAGECACHE_IO_WQS.len();
    PAGECACHE_IO_WQS[idx].enqueue(work);
}

lazy_static! {
    // Only harvest already submitted reads here. Workers which plan or
    // execute filesystem I/O may wait for these publications, including
    // final shutdown on SYSTEM_WQ and reclaim on PAGECACHE_IO_WQS.
    static ref READ_COMPLETION_WQ: Arc<WorkQueue> = WorkQueue::new("pc-read-complete");
}

pub(crate) fn init_read_completion_queue() {
    lazy_static::initialize(&READ_COMPLETION_WQ);
}

pub(crate) fn schedule_read_completion(work: Arc<Work>) {
    READ_COMPLETION_WQ.enqueue(work);
}

// Registry membership follows the mapping's lifetime, not future allocations
// or an explicit drop_caches request. A tree releases retired storage without
// retaining a vector/hash table sized for the historical peak mapping count.
struct PageCacheRegistry {
    entries: BTreeMap<u64, Weak<PageCache>>,
}

impl PageCacheRegistry {
    const fn new() -> Self {
        Self {
            entries: BTreeMap::new(),
        }
    }

    fn register(&mut self, cache: &Arc<PageCache>) {
        // Unlike the diagnostic id, instance_id cannot wrap or be reused.
        self.entries
            .insert(cache.instance_id, Arc::downgrade(cache));
    }

    fn unregister(&mut self, instance_id: u64) -> Option<Weak<PageCache>> {
        let retired = self.entries.remove(&instance_id);
        if self.entries.is_empty() {
            // BTreeMap may keep an empty root after removing its last entry.
            self.entries.clear();
        }
        retired
    }
}

fn register_page_cache(cache: &Arc<PageCache>) {
    PAGECACHE_REGISTRY.lock_irqsave().register(cache);
}

fn unregister_page_cache(cache: &PageCache) {
    let retired = PAGECACHE_REGISTRY
        .lock_irqsave()
        .unregister(cache.instance_id);
    // Keep weak-reference deallocation outside the registry lock. The Arc's
    // implicit weak reference also protects the currently running destructor.
    drop(retired);
}

pub fn list_page_caches() -> Vec<Arc<PageCache>> {
    let guard = PAGECACHE_REGISTRY.lock_irqsave();
    // Snapshot references outlive the registry lock: their final release may
    // enter PageCache::drop and acquire this same lock to unregister. A failed
    // upgrade means that destructor is already responsible for retirement.
    let caches = guard.entries.values().filter_map(Weak::upgrade).collect();
    drop(guard);
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
    writeback_domain: Option<Weak<PageCacheWritebackDomain>>,
    i_mmap_rwsem: RwSem<()>,
    invalidate_lock: RwSem<()>,
    file_vma_seq: AtomicU64,
    file_vmas: SpinLock<FileVmaIndex>,
    writeback_error: ErrSeq,
    unevictable: AtomicBool,
    kind: PageCacheKind,
    reclassify_lock: Mutex<()>,
    /// Serialize range-freeze tag scans while allowing each bounded chunk to
    /// release `invalidate_write`. Without this separate scanner lock, two
    /// epochs could interleave between chunks and a later epoch could take
    /// ownership of pages which were already dirty at the earlier caller's
    /// snapshot boundary.
    writeback_tag_scan_lock: Mutex<()>,
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
    /// Exact continuations for frozen pages redirtied behind an older
    /// Writeback incarnation. Completion removes and dispatches these without
    /// parking a shared worker.
    writeback_incarnation_retries: Mutex<Vec<TaggedWritebackIncarnationRetry>>,
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

/// A fallibly admitted remote write against one exact attached cache entry.
///
/// The entry pin prevents truncate/invalidate from replacing this incarnation.
/// Dirty membership is published while the caller holds the page write lock,
/// immediately before the infallible byte copy. Writeback also serializes its
/// page snapshot with that lock and rechecks successor membership under
/// `inner`, so it cannot observe a partially copied page or lose a redirty.
pub(crate) struct PreparedRemotePageDirty {
    cache: Arc<PageCache>,
    entry: Arc<PageEntry>,
    _pin: PageEntryPin,
    page_index: usize,
    reservation: PageDirtyReservation,
}

impl PreparedRemotePageDirty {
    pub(crate) fn publish_before_copy(&mut self, page_locked: &InnerPage) {
        let mut inner = self.cache.inner.lock();
        let current = inner
            .get_entry(self.page_index)
            .expect("prepared remote dirty entry detached while pinned");
        assert!(
            Arc::ptr_eq(&current, &self.entry),
            "prepared remote dirty entry was replaced while pinned"
        );
        PageCache::validate_page_locked_entry(&current, page_locked)
            .expect("prepared remote dirty page identity changed");
        let _ = PageCache::publish_prepared_front_dirty_locked(
            &mut inner,
            self.page_index,
            &current,
            &mut self.reservation,
        );
    }
}

#[derive(Debug)]
pub struct InnerPageCache {
    #[allow(unused)]
    id: usize,
    pages: HashMap<usize, Arc<PageEntry>>,
    // The table allocation is physically freed before its byte receipt.
    pages_charge: Option<ObjectCharge>,
    page_indices: CacheIndex,
    dirty_pages: CacheIndex,
    /// Page indices whose entries are currently in `PageState::Writeback`.
    ///
    /// This is maintained under the same `inner` lock as the entry state so
    /// range completion checks remain atomic without scanning every cached
    /// page while holding the lock.
    writeback_pages: CacheIndex,
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
    created: bool,
}

const READ_USER_BATCH_PAGES: usize = 16;

/// Pins for one bounded user read. No inode operation, invalidation guard or
/// page lock survives preparation; consuming the batch releases every pin.
#[derive(Default)]
pub(crate) struct ReadUserBatch {
    items: [Option<CopyItem>; READ_USER_BATCH_PAGES],
    len: usize,
}

impl ReadUserBatch {
    pub(crate) fn len(&self) -> usize {
        self.len
    }

    /// Attempt the resident destination fast path. The caller may fault or
    /// recover only after this consumes the batch and drops all remaining pins.
    pub(crate) fn copy_nofault(
        self,
        writer: &mut crate::syscall::user_buffer::UserBuffer<'_>,
        dst_offset: usize,
    ) -> usize {
        let mut copied = 0;
        for item in self.items.into_iter().flatten() {
            let page = item.entry.page.read();
            let source =
                unsafe { &page.as_slice()[item.page_offset..item.page_offset + item.sub_len] };
            let n = writer
                .write_prefix_to_user_nofault(dst_offset + copied, source)
                .unwrap_or(0);
            copied += n;
            if n != item.sub_len {
                break;
            }
        }
        copied
    }
}

/// Own write preparation until publication, including its failure cleanup.
/// Page locks and dirty reservations must be dropped before this owner.
struct PreparedWriteCopies<'a> {
    cache: &'a PageCache,
    items: Vec<CopyItem>,
    rollback: bool,
}

impl Drop for PreparedWriteCopies<'_> {
    fn drop(&mut self) {
        if !self.rollback {
            return;
        }
        for item in self.items.drain(..).rev() {
            let CopyItem {
                entry,
                _pin,
                page_index,
                created,
                ..
            } = item;
            drop(_pin);
            if created {
                let _ = self
                    .cache
                    .manager()
                    .discard_created_page(page_index, &entry.page);
            }
        }
    }
}

#[derive(Debug)]
pub struct PageIoWaiter {
    completion: Completion,
    state: SpinLock<PageIoWaiterState>,
}

type PageIoTerminal = Box<dyn FnOnce(Result<usize, SystemError>) + Send>;

struct PageIoWaiterState {
    result: Option<Result<usize, SystemError>>,
    terminal: Option<PageIoTerminal>,
    registered: bool,
}

impl core::fmt::Debug for PageIoWaiterState {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PageIoWaiterState")
            .field("result", &self.result)
            .field("registered", &self.registered)
            .finish()
    }
}

impl PageIoWaiter {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            completion: Completion::new(),
            state: SpinLock::new(PageIoWaiterState {
                result: None,
                terminal: None,
                registered: false,
            }),
        })
    }

    pub fn complete(&self, result: Result<usize, SystemError>) {
        let terminal = {
            let mut state = self.state.lock_irqsave();
            if state.result.is_some() {
                return;
            }
            state.result = Some(result.clone());
            state.terminal.take()
        };
        self.completion.complete();
        if let Some(terminal) = terminal {
            terminal(result);
        }
    }

    /// Register one terminal observer. It runs outside the waiter lock and
    /// must only enqueue prepared publication work, never perform or wait for I/O.
    pub fn register_terminal(&self, terminal: PageIoTerminal) -> Result<(), SystemError> {
        let ready = {
            let mut state = self.state.lock_irqsave();
            if state.registered {
                return Err(SystemError::EEXIST);
            }
            state.registered = true;
            match state.result.clone() {
                Some(result) => Some((terminal, result)),
                None => {
                    state.terminal = Some(terminal);
                    None
                }
            }
        };
        if let Some((terminal, result)) = ready {
            terminal(result);
        }
        Ok(())
    }

    pub fn wait(&self) -> Result<usize, SystemError> {
        self.completion.wait_for_completion()?;
        match self.state.lock_irqsave().result.as_ref() {
            Some(Ok(len)) => Ok(*len),
            Some(Err(e)) => Err(e.clone()),
            None => Err(SystemError::EIO),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PageCacheFaultState {
    pub(crate) cached: bool,
    pub(crate) ready: bool,
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
    storage: OwnedBox<PageEntryState>,
}

/// Keeps removed entry/index storage alive until the reclaimer has released
/// all cache and global registry guards. Its final drop may release charges.
pub(crate) struct RetiredPageCacheEntry {
    entry: Arc<PageEntry>,
    backend: Option<Arc<dyn PageCacheBackend>>,
}

impl Drop for RetiredPageCacheEntry {
    fn drop(&mut self) {
        self.entry.account_remove();
        if let Some(backend) = &self.backend {
            backend.release_page();
        }
    }
}

impl core::ops::Deref for PageEntry {
    type Target = PageEntryState;
    fn deref(&self) -> &Self::Target {
        &self.storage
    }
}

struct PageEntryState {
    page: Arc<Page>,
    index_spares: SpinLock<[Option<OwnedBox<CacheIndexNode>>; 3]>,
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

struct CacheTableCapacity {
    pages: HashMap<usize, Arc<PageEntry>>,
    charge: Option<ObjectCharge>,
}

/// Backend quota reserved for an as-yet invisible range. Successful cache
/// publication transfers each unit to remove_entry; every other exit refunds
/// it before any reader can attach to a partial Loading range.
struct BackendPageReservation {
    backend: Option<Arc<dyn PageCacheBackend>>,
    remaining: usize,
}

impl BackendPageReservation {
    fn prepare(
        backend: Option<Arc<dyn PageCacheBackend>>,
        count: usize,
    ) -> Result<Self, SystemError> {
        let mut permit = Self {
            backend,
            remaining: 0,
        };
        if let Some(backend) = permit.backend.as_ref() {
            for _ in 0..count {
                backend.reserve_page()?;
                permit.remaining += 1;
            }
        }
        Ok(permit)
    }

    fn commit(&mut self) {
        self.remaining = 0;
    }
}

impl Drop for BackendPageReservation {
    fn drop(&mut self) {
        if let Some(backend) = self.backend.as_ref() {
            for _ in 0..self.remaining {
                backend.release_page();
            }
        }
    }
}

impl Drop for CacheTableCapacity {
    fn drop(&mut self) {
        drop(mem::take(&mut self.pages));
        drop(self.charge.take());
    }
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
    fn wait(&self) -> Result<(), MemoryAllocationError> {
        PageCacheManager::wait_writeback_entry(self.entry.clone())
            .map_err(MemoryAllocationError::System)
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
    pub(crate) fn try_commit_page_pinned(
        &self,
        page_index: usize,
        owner: &Arc<MemoryAccount>,
        creator: &Arc<ObjectAccount>,
    ) -> Result<PageCachePagePin, MemoryAllocationError> {
        self.upgrade()?
            .try_get_or_create_page_pinned(page_index, true, owner, creator)
    }

    pub(crate) fn try_commit_overwrite_pinned(
        &self,
        page_index: usize,
        owner: &Arc<MemoryAccount>,
        creator: &Arc<ObjectAccount>,
    ) -> Result<PageCachePagePin, MemoryAllocationError> {
        self.upgrade()?
            .try_get_or_create_page_pinned(page_index, false, owner, creator)
    }
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

    pub(crate) fn try_commit_page_with<F>(
        &self,
        page_index: usize,
        owner: &Arc<MemoryAccount>,
        creator: &Arc<ObjectAccount>,
        fill: F,
    ) -> Result<Arc<Page>, MemoryAllocationError>
    where
        F: FnOnce(usize, &mut [u8]) -> Result<usize, SystemError>,
    {
        self.upgrade()?
            .try_get_or_create_page_with(page_index, owner, creator, fill)
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

    pub(crate) fn try_commit_page_for_write_with<F>(
        &self,
        page_index: usize,
        owner: &Arc<MemoryAccount>,
        creator: &Arc<ObjectAccount>,
        fill: F,
    ) -> Result<Arc<Page>, MemoryAllocationError>
    where
        F: FnOnce(usize, &mut [u8]) -> Result<usize, SystemError>,
    {
        self.upgrade()?
            .try_get_or_create_page_for_write_with(page_index, owner, creator, fill)
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

    /// Populate an inclusive range with zero-filled holes, preserving existing pages.
    /// Callers serialize size changes/truncation and publish EOF only on success.
    /// On failure, discard only pages created here that have not been acquired,
    /// mapped or dirtied by another user; rollback is not a truncate operation.
    pub fn preallocate_range(&self, first: usize, last: usize) -> Result<(), SystemError> {
        let task = crate::process::ProcessManager::current_pcb();
        let owner = crate::cgroup::memory::account_for_task(&task);
        let creator = crate::cgroup::memory::object_for_task(&task);
        self.try_preallocate_range(first, last, &owner, &creator)
            .map_err(PageCache::legacy_allocation_error)
    }

    pub(crate) fn try_preallocate_range(
        &self,
        first: usize,
        last: usize,
        owner: &Arc<MemoryAccount>,
        creator: &Arc<ObjectAccount>,
    ) -> Result<(), MemoryAllocationError> {
        let cache = self.upgrade()?;
        let missing_pages = self.missing_pages_in_range(first, last)?;
        let mut created: Vec<(usize, Arc<Page>)> = Vec::new();
        created
            .try_reserve_exact(missing_pages)
            .map_err(|_| SystemError::ENOMEM)?;

        for page_index in first..=last {
            match cache.try_get_or_create_entry_pinned(page_index, false, owner, creator) {
                Ok((entry, entry_pin, was_created)) => {
                    let pin = PageCachePagePin::new(entry.page.clone(), entry_pin);
                    if was_created {
                        if created.len() == created.capacity() && created.try_reserve(1).is_err() {
                            let current_page = pin.page();
                            drop(pin);
                            let _ = self.discard_created_page(page_index, &current_page);
                            for (created_index, created_page) in created.into_iter().rev() {
                                let _ = self.discard_created_page(created_index, &created_page);
                            }
                            return Err(SystemError::ENOMEM.into());
                        }
                        created.push((page_index, pin.page()));
                    }
                }
                Err(error) => {
                    for (created_index, created_page) in created.into_iter().rev() {
                        let _ = self.discard_created_page(created_index, &created_page);
                    }
                    return Err(error);
                }
            }
        }

        Ok(())
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

    pub(crate) fn page_mkwrite_retry_wait(
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
                inner.remove_entry(page_index)
            } else {
                None
            }
        };
        drop(cache.detach_dirty_retention_if_idle());
        Ok(removed.map(|entry| entry.page.clone()))
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
            guard.remove_entry(page_index)
        };
        drop(page_guard);
        let Some(page) = removed else {
            return Ok(false);
        };
        cache.discard_unlinked_page(&page.page);
        drop(cache.detach_dirty_retention_if_idle());
        Ok(true)
    }

    /// The caller retains an exclusive page guard through cache and registry
    /// retirement. NOIO must skip busy entries rather than wait for their owner.
    pub(crate) fn try_remove_clean_page_for_reclaim(
        &self,
        page_index: usize,
        expected_page: &Arc<Page>,
        page_guard: &InnerPage,
    ) -> Option<RetiredPageCacheEntry> {
        let cache = self.upgrade().ok()?;
        if cache.is_shmem() || cache.mapping_unevictable() {
            return None;
        }
        let mut inner = cache.inner.try_lock().ok()?;
        let entry = inner.pages.get(&page_index)?;
        if !Arc::ptr_eq(&entry.page, expected_page)
            || entry.active_users() != 0
            || entry.state() != PageState::UpToDate
            || page_guard.map_count() != 0
            || page_guard.flags().intersects(
                PageFlags::PG_DIRTY | PageFlags::PG_WRITEBACK | PageFlags::PG_UNEVICTABLE,
            )
            || inner.dirty_pages.contains(&page_index)
            || inner.writeback_pages.contains(&page_index)
        {
            return None;
        }
        // Membership changes under the cache lock, but backend callbacks and
        // entry/charge destruction belong after global registry retirement.
        inner.set_index(page_index, 0, false);
        inner.set_index(page_index, 1, false);
        inner.set_index(page_index, 2, false);
        inner
            .pages
            .remove(&page_index)
            .map(|entry| RetiredPageCacheEntry {
                entry,
                backend: inner.accounting_backend.clone(),
            })
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
            PageState::Loading | PageState::Dirty | PageState::Writeback | PageState::Error
        ) {
            return Ok(None);
        }
        // Keep page -> cache locked through membership removal, as in
        // discard_created_page. Otherwise dirty publication or rmap insertion
        // can invalidate this check before the later cache-only commit.
        let page_guard = entry.page.read();
        let page_reclaimable = !page_guard
            .flags()
            .intersects(PageFlags::PG_DIRTY | PageFlags::PG_WRITEBACK | PageFlags::PG_UNEVICTABLE)
            && page_guard.map_count() == 0;
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
            || guard.dirty_pages.contains(&page_index)
            || guard.writeback_pages.contains(&page_index)
        {
            return Ok(None);
        }
        let removed = guard.remove_entry(page_index);
        drop(guard);
        drop(page_guard);
        Ok(removed.map(|entry| entry.page.clone()))
    }
}

impl core::fmt::Debug for PageCacheManager {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PageCacheManager").finish()
    }
}

impl PageEntry {
    /// Legacy selftest fixture construction; real cache birth uses try_new
    /// before publication and preserves its typed admission error.
    fn new(page: Arc<Page>, state: PageState) -> Self {
        let creator =
            crate::cgroup::memory::object_for_task(&crate::process::ProcessManager::current_pcb());
        Self::try_new(page, state, &creator).expect("selftest entry preparation failed")
    }

    fn try_new(
        page: Arc<Page>,
        state: PageState,
        creator: &Arc<ObjectAccount>,
    ) -> Result<Self, MemoryAllocationError> {
        let instance_id = PAGE_CACHE_ENTRY_INSTANCE_ID
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                current.checked_add(1)
            })
            .map_err(|_| MemoryAllocationError::System(SystemError::EOVERFLOW))?;
        let nodes = [
            Some(CacheIndexNode::try_new(creator)?),
            Some(CacheIndexNode::try_new(creator)?),
            Some(CacheIndexNode::try_new(creator)?),
        ];
        let storage = OwnedBox::prepare(creator, KernelByteKind::SlabUnreclaimable)
            .map_err(MemoryAllocationError::Charge)?
            .try_init(PageEntryState {
                page,
                index_spares: SpinLock::new(nodes),
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
            })
            .map_err(|_| MemoryAllocationError::System(SystemError::ENOMEM))?;
        Ok(Self { storage })
    }

    fn take_index_node(&self, slot: usize, index: usize) -> OwnedBox<CacheIndexNode> {
        let mut node = self.index_spares.lock_irqsave()[slot]
            .take()
            .expect("entry index already linked");
        node.key = index;
        node
    }

    fn return_index_node(&self, slot: usize, node: OwnedBox<CacheIndexNode>) {
        let mut spares = self.index_spares.lock_irqsave();
        assert!(spares[slot].is_none(), "entry index returned twice");
        spares[slot] = Some(node);
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
    /// Commit a capacity prepared without the cache lock. A racing insertion
    /// can invalidate its size, never the current live entries or accounting.
    fn install_capacity(
        &mut self,
        prepared: &mut Option<CacheTableCapacity>,
        additional: usize,
    ) -> Result<Option<CacheTableCapacity>, ()> {
        let required = self.pages.len().checked_add(additional).ok_or(())?;
        if required <= self.pages.capacity() {
            return Ok(None);
        }
        let candidate = prepared.as_mut().ok_or(())?;
        if required > candidate.pages.capacity() {
            return Err(());
        }
        let mut retired = CacheTableCapacity {
            pages: mem::take(&mut self.pages),
            charge: self.pages_charge.take(),
        };
        for (key, entry) in retired.pages.drain() {
            candidate.pages.insert(key, entry);
        }
        self.pages = mem::take(&mut candidate.pages);
        self.pages_charge = candidate.charge.take();
        Ok(Some(retired))
    }
    fn update_index(
        index: &mut CacheIndex,
        entry: &PageEntry,
        slot: usize,
        offset: usize,
        linked: bool,
    ) {
        if linked {
            if !index.contains(&offset) {
                index.insert(entry.take_index_node(slot, offset));
            }
        } else if let Some(node) = index.remove(&offset) {
            entry.return_index_node(slot, node);
        }
    }

    fn set_index(&mut self, offset: usize, slot: usize, linked: bool) {
        let entry = self
            .pages
            .get(&offset)
            .expect("ordered index has no cache entry");
        let index = match slot {
            0 => &mut self.page_indices,
            1 => &mut self.dirty_pages,
            2 => &mut self.writeback_pages,
            _ => unreachable!(),
        };
        Self::update_index(index, entry, slot, offset, linked);
    }

    fn new(
        page_cache_ref: Weak<PageCache>,
        id: usize,
        kind: PageCacheKind,
        accounting_backend: Option<Arc<dyn PageCacheBackend>>,
    ) -> InnerPageCache {
        Self {
            id,
            pages: HashMap::new(),
            pages_charge: None,
            page_indices: CacheIndex::new(),
            dirty_pages: CacheIndex::new(),
            writeback_pages: CacheIndex::new(),
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

    fn remove_entry(&mut self, offset: usize) -> Option<Arc<PageEntry>> {
        if !self.pages.contains_key(&offset) {
            return None;
        }
        self.set_index(offset, 0, false);
        self.set_index(offset, 1, false);
        self.set_index(offset, 2, false);
        let entry = self.pages.remove(&offset).unwrap();
        entry.account_remove();
        if let Some(backend) = self.accounting_backend.as_ref() {
            backend.release_page();
        }
        Some(entry)
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
        if let Some(backend) = self.accounting_backend.as_ref() {
            backend.reserve_page()?;
        }
        self.insert_entry_reserved(offset, entry);
        Ok(())
    }

    /// The caller owns an unpublished backend permit and has prepared table
    /// capacity. No admission or metadata allocation can fail in this commit.
    fn insert_entry_reserved(&mut self, offset: usize, entry: Arc<PageEntry>) {
        let mapping_unevictable = self
            .page_cache_ref
            .upgrade()
            .is_some_and(|cache| cache.mapping_unevictable());
        match self.pages.entry(offset) {
            Entry::Vacant(slot) => {
                entry.account_insert(self.kind, mapping_unevictable);
                slot.insert(entry);
            }
            Entry::Occupied(_) => panic!("page-cache insert requires a vacant slot"),
        }
        self.set_index(offset, 0, true);
    }

    /// Insert an intrinsically unevictable, preallocated page without adding
    /// it to the ordered file-cache index. Ring backings are looked up by exact
    /// page offset and are never candidates for range writeback/reclaim; not
    /// creating a BTree node also keeps their user-sized metadata fallible.
    fn insert_preallocated_entry(
        &mut self,
        offset: usize,
        entry: Arc<PageEntry>,
    ) -> Result<(), SystemError> {
        match self.pages.entry(offset) {
            Entry::Vacant(slot) => {
                if let Some(backend) = self.accounting_backend.as_ref() {
                    backend.reserve_page()?;
                }
                entry.account_insert(self.kind, true);
                slot.insert(entry);
                Ok(())
            }
            Entry::Occupied(_) => Err(SystemError::EEXIST),
        }
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
        // Accounting/backend callbacks and final Arc destruction must never
        // extend either global page-registry critical section.
        for entry in self.pages.values() {
            entry.account_remove();
            if let Some(backend) = self.accounting_backend.as_ref() {
                backend.release_page();
            }
        }
        detach_pages_from_manager_batched(self.pages.values().map(|entry| &entry.page));
        detach_pages_from_reclaimer_batched(self.pages.values().map(|entry| &entry.page));
    }
}

impl PageCache {
    fn try_get_or_create_page_pinned(
        &self,
        page_index: usize,
        populate_backend: bool,
        owner: &Arc<MemoryAccount>,
        creator: &Arc<ObjectAccount>,
    ) -> Result<PageCachePagePin, MemoryAllocationError> {
        loop {
            let (entry, _) = self.try_get_or_create_entry_with_status(
                page_index,
                populate_backend,
                owner,
                creator,
            )?;
            let inner = self.inner.lock();
            if inner
                .pages
                .get(&page_index)
                .is_some_and(|current| Arc::ptr_eq(current, &entry))
                && entry.state().is_ready()
            {
                let pin = entry.pin();
                drop(inner);
                return Ok(PageCachePagePin::new(entry.page.clone(), pin));
            }
        }
    }
    fn prepare_table_capacity(
        &self,
        additional: usize,
        creator: &Arc<ObjectAccount>,
    ) -> Result<Option<CacheTableCapacity>, MemoryAllocationError> {
        let required = {
            let inner = self.inner.lock();
            let required = inner
                .pages
                .len()
                .checked_add(additional)
                .ok_or(SystemError::EOVERFLOW)?;
            if required <= inner.pages.capacity() {
                return Ok(None);
            }
            required
        };
        let mut pages = HashMap::new();
        pages
            .try_reserve(required)
            .map_err(|_| MemoryAllocationError::System(SystemError::ENOMEM))?;
        // Only inspect the pinned library's actual allocation layout. No raw
        // bucket access or copied private capacity/layout formula is used.
        let (_, layout) = pages.raw_table().allocation_info();
        let charge = match creator.try_charge_allocation(KernelByteKind::SlabUnreclaimable, layout)
        {
            Ok(charge) => charge,
            Err(error) => {
                drop(pages);
                return Err(MemoryAllocationError::Charge(error));
            }
        };
        Ok(Some(CacheTableCapacity {
            pages,
            charge: Some(charge),
        }))
    }
    // Lock order: page_cache -> page_manager -> page_reclaimer.
    // Avoid holding page_cache lock while acquiring page_manager when possible.
    pub fn new_file(
        inode: Weak<dyn IndexNode>,
        backend: Arc<dyn PageCacheBackend>,
        domain: &Arc<PageCacheWritebackDomain>,
    ) -> Result<Arc<PageCache>, SystemError> {
        Self::new_filesystem_mapping(inode, backend, PageCacheKind::File, domain)
    }

    pub fn new_filesystem_shmem(
        inode: Weak<dyn IndexNode>,
        backend: Arc<dyn PageCacheBackend>,
        domain: &Arc<PageCacheWritebackDomain>,
    ) -> Result<Arc<PageCache>, SystemError> {
        Self::new_filesystem_mapping(inode, backend, PageCacheKind::Shmem, domain)
    }

    fn new_filesystem_mapping(
        inode: Weak<dyn IndexNode>,
        backend: Arc<dyn PageCacheBackend>,
        kind: PageCacheKind,
        domain: &Arc<PageCacheWritebackDomain>,
    ) -> Result<Arc<PageCache>, SystemError> {
        let cache = Self::new_with_kind(
            Some(inode),
            Some(backend),
            kind,
            Some(Arc::downgrade(domain)),
        );
        domain.register(&cache)?;
        Ok(cache)
    }

    pub fn new_unowned(
        inode: Option<Weak<dyn IndexNode>>,
        backend: Option<Arc<dyn PageCacheBackend>>,
    ) -> Arc<PageCache> {
        Self::new_with_kind(inode, backend, PageCacheKind::File, None)
    }

    pub fn new_shmem(
        inode: Option<Weak<dyn IndexNode>>,
        backend: Option<Arc<dyn PageCacheBackend>>,
    ) -> Arc<PageCache> {
        Self::new_with_kind(inode, backend, PageCacheKind::Shmem, None)
    }

    fn new_with_kind(
        inode: Option<Weak<dyn IndexNode>>,
        backend: Option<Arc<dyn PageCacheBackend>>,
        kind: PageCacheKind,
        writeback_domain: Option<Weak<PageCacheWritebackDomain>>,
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
            writeback_domain,
            i_mmap_rwsem: RwSem::new(()),
            invalidate_lock: RwSem::new(()),
            file_vma_seq: AtomicU64::new(0),
            file_vmas: SpinLock::new(FileVmaIndex::default()),
            writeback_error: ErrSeq::new(),
            unevictable: AtomicBool::new(false),
            kind,
            reclassify_lock: Mutex::new(()),
            writeback_tag_scan_lock: Mutex::new(()),
            tagged_writeback_lock: Mutex::new(()),
            tagged_writeback_progress: AtomicU64::new(0),
            tagged_writeback_wait: WaitQueue::default(),
            tagged_writeback_submissions: Mutex::new(Vec::new()),
            writeback_incarnation_retries: Mutex::new(Vec::new()),
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
        if let Some(domain) = self.writeback_domain.as_ref().and_then(Weak::upgrade) {
            domain.record_writeback_error(error);
        }
    }

    /// Nonblocking revalidation for a caller holding invalidate_write.
    /// Flush outside that lock, then reject any concurrent redirty before
    /// discarding clean pages for direct publication.
    pub(crate) fn has_dirty_or_writeback_pages_in_range(&self, first: usize, last: usize) -> bool {
        let inner = self.inner.lock();
        inner.dirty_pages.range(first..=last).next().is_some()
            || inner.writeback_pages.range(first..=last).next().is_some()
    }

    pub fn has_dirty_or_writeback_pages(&self) -> bool {
        let inner = self.inner.lock();
        !inner.dirty_pages.is_empty() || !inner.writeback_pages.is_empty()
    }

    fn writeback_executor(&self) -> Result<Arc<WritebackExecutor>, SystemError> {
        match self.writeback_domain.as_ref() {
            Some(domain) => domain
                .upgrade()
                .ok_or(SystemError::ESTALE)?
                .writeback_executor(),
            None => writeback::unowned_writeback_executor(),
        }
    }

    pub fn try_acquire_domain_io(&self) -> Result<Option<PageCacheDomainIoPermit>, SystemError> {
        self.try_acquire_domain_io_classified()
            .map_err(|error| match error {
                PageCacheDomainIoAdmissionError::Closed => SystemError::ESTALE,
                PageCacheDomainIoAdmissionError::Unavailable(error) => error,
            })
    }

    pub(crate) fn try_acquire_domain_io_classified(
        &self,
    ) -> Result<Option<PageCacheDomainIoPermit>, PageCacheDomainIoAdmissionError> {
        match self.writeback_domain.as_ref() {
            None => Ok(None),
            Some(domain) => {
                let domain =
                    domain
                        .upgrade()
                        .ok_or(PageCacheDomainIoAdmissionError::Unavailable(
                            SystemError::ESTALE,
                        ))?;
                domain.try_acquire_io_classified().map(Some)
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

    /// Cache-only metadata may inspect membership, but must not wait, fill
    /// pages or call the backing filesystem while doing so.
    pub(crate) fn try_pages_count(&self) -> Result<usize, SystemError> {
        Ok(self
            .inner
            .try_lock()
            .map_err(|_| SystemError::EAGAIN_OR_EWOULDBLOCK)?
            .pages_count())
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

            // Match reclaim's page -> cache commit. A newly mapped or dirtied
            // page must not be detached after a stale unlocked flag check.
            let page_guard = entry.page.read();
            let page_reclaimable = !page_guard.flags().intersects(
                PageFlags::PG_DIRTY | PageFlags::PG_WRITEBACK | PageFlags::PG_UNEVICTABLE,
            ) && page_guard.map_count() == 0;
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
                || guard.dirty_pages.contains(&page_index)
                || guard.writeback_pages.contains(&page_index)
            {
                return None;
            }
            let removed = guard.remove_entry(page_index);
            drop(guard);
            drop(page_guard);
            return removed.map(|entry| entry.page.clone());
        }
    }

    fn evict_clean_pages_for_invalidate(&self, range: Option<(usize, usize)>) -> usize {
        let mut evicted = 0;
        for page_index in self.clean_evict_indices(range) {
            if let Some(page) = self.remove_clean_page_candidate(page_index, true) {
                detach_pages_from_reclaimer_batched(core::iter::once(&page));
                let removed = page_manager_lock().remove_page_if_same(&page);
                drop(removed);
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

    pub(crate) fn is_shmem(&self) -> bool {
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
                    drop(page_guard);
                    entry.account_unevictable_if_needed();
                    drop(guard);
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
        if mapping_unevictable {
            let mut page_guard = entry.page.write();
            let was_unevictable = page_guard.flags().contains(PageFlags::PG_UNEVICTABLE);
            if !was_unevictable {
                page_guard.add_flags(PageFlags::PG_UNEVICTABLE);
            }
            drop(page_guard);
            return;
        }

        entry.unaccount_unevictable_if_needed();
        let mut page_guard = entry.page.write();
        let was_unevictable = page_guard.flags().contains(PageFlags::PG_UNEVICTABLE);
        if was_unevictable && !page_guard.has_unevictable_source() {
            page_guard.remove_flags(PageFlags::PG_UNEVICTABLE);
        }
    }

    fn allocate_page(
        &self,
        page_cache_ref: Weak<PageCache>,
        page_index: usize,
    ) -> Result<Arc<Page>, SystemError> {
        let owner =
            crate::cgroup::memory::account_for_task(&crate::process::ProcessManager::current_pcb());
        self.allocate_page_with_owner(page_cache_ref, page_index, &owner)
            .map_err(Self::legacy_allocation_error)
    }

    fn legacy_allocation_error(error: MemoryAllocationError) -> SystemError {
        match error {
            MemoryAllocationError::Charge(_) => SystemError::ENOMEM,
            MemoryAllocationError::System(error) => error,
        }
    }

    fn allocate_page_with_owner(
        &self,
        page_cache_ref: Weak<PageCache>,
        page_index: usize,
        owner: &Arc<MemoryAccount>,
    ) -> Result<Arc<Page>, MemoryAllocationError> {
        let kind = if self.is_shmem() {
            MemoryKind::Shmem
        } else {
            MemoryKind::File
        };
        let charge = owner
            .try_charge(kind, 1)
            .map_err(MemoryAllocationError::Charge)?;
        crate::mm::page::allocate_registered_data_page(
            PageType::File(FileMapInfo {
                page_cache: page_cache_ref,
                index: page_index,
            }),
            self.page_flags(),
            Some(charge),
        )
        .map_err(MemoryAllocationError::System)
    }

    fn prepare_entry(
        &self,
        page: &Arc<Page>,
        state: PageState,
        creator: &Arc<ObjectAccount>,
    ) -> Result<Arc<PageEntry>, MemoryAllocationError> {
        let result = PageEntry::try_new(page.clone(), state, creator).and_then(|entry| {
            Arc::try_new(entry).map_err(|_| MemoryAllocationError::System(SystemError::ENOMEM))
        });
        if result.is_err() {
            self.discard_unlinked_page(page);
        }
        result
    }

    fn publish_prepared_entry(
        &self,
        page_index: usize,
        prepared_entry: &Arc<PageEntry>,
        creator: &Arc<ObjectAccount>,
    ) -> Result<(Arc<PageEntry>, bool), MemoryAllocationError> {
        let mut capacity = None;
        loop {
            if capacity.is_none() {
                match self.prepare_table_capacity(1, creator) {
                    Ok(prepared) => capacity = prepared,
                    Err(error) => {
                        self.discard_unlinked_page(&prepared_entry.page);
                        return Err(error);
                    }
                }
            }
            let mut inner = self.inner.lock();
            if let Some(current) = inner.get_entry(page_index) {
                drop(inner);
                self.discard_unlinked_page(&prepared_entry.page);
                return Ok((current, false));
            }
            let retired = match inner.install_capacity(&mut capacity, 1) {
                Ok(retired) => retired,
                Err(()) => {
                    drop(inner);
                    drop(capacity.take());
                    continue;
                }
            };
            let inserted = inner.insert_entry(page_index, prepared_entry.clone());
            drop(inner);
            drop(retired);
            if let Err(error) = inserted {
                self.discard_unlinked_page(&prepared_entry.page);
                return Err(error.into());
            }
            return Ok((prepared_entry.clone(), true));
        }
    }

    fn publish_prepared_range<'a, I>(
        &self,
        entries: I,
        count: usize,
        creator: &Arc<ObjectAccount>,
    ) -> Result<(), MemoryAllocationError>
    where
        I: Iterator<Item = (usize, &'a Arc<PageEntry>)> + Clone,
    {
        let backend = self.inner.lock().accounting_backend.clone();
        let mut permit = BackendPageReservation::prepare(backend, count)?;
        loop {
            let mut capacity = self.prepare_table_capacity(count, creator)?;
            let mut inner = self.inner.lock();
            if entries
                .clone()
                .any(|(index, _)| inner.pages.contains_key(&index))
            {
                drop(inner);
                return Err(SystemError::EEXIST.into());
            }
            let retired = match inner.install_capacity(&mut capacity, count) {
                Ok(retired) => retired,
                Err(()) => {
                    drop(inner);
                    drop(capacity);
                    continue;
                }
            };
            for (index, entry) in entries.clone() {
                inner.insert_entry_reserved(index, entry.clone());
            }
            permit.commit();
            drop(inner);
            drop(retired);
            return Ok(());
        }
    }

    fn populate_page_from_backend(
        &self,
        page_index: usize,
        page: &Arc<Page>,
    ) -> Result<(), SystemError> {
        let _domain_io = self.try_acquire_domain_io()?;
        let backend = self.backend();
        if let Some(backend) = backend {
            let waiter = PageIoWaiter::new();
            backend.read_page_async(page_index, page, &waiter);
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
        let owner =
            crate::cgroup::memory::account_for_task(&crate::process::ProcessManager::current_pcb());
        let creator =
            crate::cgroup::memory::object_for_task(&crate::process::ProcessManager::current_pcb());
        self.try_get_or_create_entry_with_status(page_index, populate_backend, &owner, &creator)
            .map_err(Self::legacy_allocation_error)
    }

    fn try_get_or_create_entry_with_status(
        &self,
        page_index: usize,
        populate_backend: bool,
        owner: &Arc<MemoryAccount>,
        creator: &Arc<ObjectAccount>,
    ) -> Result<(Arc<PageEntry>, bool), MemoryAllocationError> {
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
                return Err(SystemError::EIO.into());
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
                let page = self.allocate_page_with_owner(
                    page_cache_ref.expect("page_cache_ref should exist"),
                    page_index,
                    owner,
                )?;
                let prepared_entry = self.prepare_entry(&page, PageState::Loading, creator)?;
                self.publish_prepared_entry(page_index, &prepared_entry, creator)?
            }
        };

        if !need_populate {
            let state = entry.state();
            if state.is_ready() {
                return Ok((entry, false));
            }
            if state == PageState::Error {
                return Err(SystemError::EIO.into());
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
                Err(e.into())
            }
        }
    }

    fn get_or_create_entry_pinned(
        &self,
        page_index: usize,
        populate_backend: bool,
    ) -> Result<(Arc<PageEntry>, PageEntryPin, bool), SystemError> {
        let task = crate::process::ProcessManager::current_pcb();
        let owner = crate::cgroup::memory::account_for_task(&task);
        let creator = crate::cgroup::memory::object_for_task(&task);
        self.try_get_or_create_entry_pinned(page_index, populate_backend, &owner, &creator)
            .map_err(Self::legacy_allocation_error)
    }

    fn try_get_or_create_entry_pinned(
        &self,
        page_index: usize,
        populate_backend: bool,
        owner: &Arc<MemoryAccount>,
        creator: &Arc<ObjectAccount>,
    ) -> Result<(Arc<PageEntry>, PageEntryPin, bool), MemoryAllocationError> {
        loop {
            let (entry, created) = self.try_get_or_create_entry_with_status(
                page_index,
                populate_backend,
                owner,
                creator,
            )?;
            let guard = self.inner.lock();
            let Some(current) = guard.get_entry(page_index) else {
                continue;
            };
            if !Arc::ptr_eq(&current, &entry) || !entry.state().is_ready() {
                continue;
            }
            let pin = entry.pin();
            return Ok((entry, pin, created));
        }
    }

    fn remove_failed_entry(&self, page_index: usize, entry: &Arc<PageEntry>) {
        let mut guard = self.inner.lock();
        if let Some(current) = guard.get_entry(page_index) {
            if Arc::ptr_eq(&current, entry) {
                guard.remove_entry(page_index);
            }
        }
        drop(guard);
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
            guard.remove_entry(page_index)
        };

        if let Some(page) = removed {
            self.discard_unlinked_page(&page.page);
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
            guard.remove_entry(page_index)
        };
        if let Some(page) = removed {
            self.discard_unlinked_page(&page.page);
            true
        } else {
            false
        }
    }

    fn discard_unlinked_page(&self, page: &Arc<Page>) {
        let can_remove_from_manager = {
            let mut page_guard = page.write();
            page_guard.clear_unlinked_file_mapping_unevictable();
            page_guard.can_deallocate()
        };
        if can_remove_from_manager {
            let removed = page_manager_lock().remove_page_if_same(page);
            drop(removed);
        }
        detach_pages_from_reclaimer_batched(core::iter::once(page));
    }

    fn start_async_read(&self, page_index: usize) -> Result<(), SystemError> {
        let task = crate::process::ProcessManager::current_pcb();
        let owner = crate::cgroup::memory::account_for_task(&task);
        let creator = crate::cgroup::memory::object_for_task(&task);
        self.try_start_async_read(page_index, &owner, &creator)
            .map_err(Self::legacy_allocation_error)
    }

    fn try_start_async_read(
        &self,
        page_index: usize,
        owner: &Arc<MemoryAccount>,
        creator: &Arc<ObjectAccount>,
    ) -> Result<(), MemoryAllocationError> {
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

        let domain_io = self.try_acquire_domain_io()?;

        let prepared = {
            let guard = self.inner.lock();
            if guard.get_entry(page_index).is_some() {
                return Ok(());
            }
            drop(guard);
            let page = self.allocate_page_with_owner(
                page_cache_ref.expect("page_cache_ref should exist"),
                page_index,
                owner,
            )?;
            self.prepare_entry(&page, PageState::Loading, creator)?
        };

        let backend = self.backend();
        let inode = self.inode();
        let entry_clone = prepared.clone();
        let page = prepared.page.clone();
        let source_page = page.clone();
        let waiter = PageIoWaiter::new();

        let work_state = Arc::new(SpinLock::new(Some((domain_io, None))));
        let terminal_state = work_state.clone();
        let work = Work::new(move || {
            let Some((_domain_io, read_len)) = work_state.lock_irqsave().take() else {
                return;
            };
            let read_len = read_len.unwrap_or(Err(SystemError::EIO));

            match read_len {
                Ok(len) if len <= MMArch::PAGE_SIZE => {
                    if len < MMArch::PAGE_SIZE {
                        let mut guard = page.write();
                        let dst = unsafe { guard.as_slice_mut() };
                        dst[len..MMArch::PAGE_SIZE].fill(0);
                    }
                    page.write().add_flags(PageFlags::PG_UPTODATE);
                    entry_clone.set_state(PageState::UpToDate);
                }
                _ => {
                    page.write().add_flags(PageFlags::PG_ERROR);
                    entry_clone.set_state(PageState::Error);
                }
            }
            entry_clone.wait_queue.wake_all();
        });
        waiter.register_terminal(Box::new(move |result| {
            if let Some((_, read_len)) = terminal_state.lock_irqsave().as_mut() {
                *read_len = Some(result);
            }
            schedule_read_completion(work);
        }))?;
        let (entry, created) = self.publish_prepared_entry(page_index, &prepared, creator)?;
        if !created {
            return Ok(());
        }
        self.reconcile_entry_unevictable_for_insert(&entry);
        if let Some(backend) = backend {
            backend.read_page_async(page_index, &source_page, &waiter);
        } else {
            let result = if let Some(inode) = inode.and_then(|inode| inode.upgrade()) {
                match page_index.checked_mul(MMArch::PAGE_SIZE) {
                    Some(offset) => {
                        let mut guard = source_page.write();
                        inode.read_sync(offset, unsafe { guard.as_slice_mut() })
                    }
                    None => Err(SystemError::EOVERFLOW),
                }
            } else {
                Err(SystemError::EIO)
            };
            waiter.complete(result);
        }
        Ok(())
    }

    pub fn is_page_ready(&self, page_index: usize) -> bool {
        self.inner.lock().is_page_ready(page_index)
    }

    /// A cache miss is distinct from waiting for an existing Loading/Error
    /// entry. Capture both predicates under one lock without pinning entries
    /// or exposing their lifetime to the fault caller.
    pub(crate) fn fault_page_state(&self, page_index: usize) -> PageCacheFaultState {
        let inner = self.inner.lock();
        match inner.pages.get(&page_index) {
            Some(entry) => PageCacheFaultState {
                cached: true,
                ready: entry.state().is_ready(),
            },
            None => PageCacheFaultState {
                cached: false,
                ready: false,
            },
        }
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
        let task = crate::process::ProcessManager::current_pcb();
        let owner = crate::cgroup::memory::account_for_task(&task);
        let creator = crate::cgroup::memory::object_for_task(&task);
        self.try_get_or_create_page_with(page_index, &owner, &creator, fill)
            .map_err(Self::legacy_allocation_error)
    }

    fn try_get_or_create_page_with<F>(
        &self,
        page_index: usize,
        owner: &Arc<MemoryAccount>,
        creator: &Arc<ObjectAccount>,
        fill: F,
    ) -> Result<Arc<Page>, MemoryAllocationError>
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
                return Err(SystemError::EIO.into());
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
                let page = self.allocate_page_with_owner(
                    page_cache_ref.expect("page_cache_ref should exist"),
                    page_index,
                    owner,
                )?;
                let prepared = self.prepare_entry(&page, PageState::Loading, creator)?;
                self.publish_prepared_entry(page_index, &prepared, creator)?
            }
        };

        if !need_populate {
            let state = entry.state();
            if state.is_ready() {
                return Ok(entry.page.clone());
            }
            if state == PageState::Error {
                return Err(SystemError::EIO.into());
            }
            return entry.wait_ready().map_err(MemoryAllocationError::System);
        }
        self.reconcile_entry_unevictable_for_insert(&entry);

        let populate_result = {
            let mut tmp = [0u8; MMArch::PAGE_SIZE];
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
                Err(e.into())
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
        let task = crate::process::ProcessManager::current_pcb();
        let owner = crate::cgroup::memory::account_for_task(&task);
        let creator = crate::cgroup::memory::object_for_task(&task);
        self.try_get_or_create_page_for_write_with(page_index, &owner, &creator, fill)
            .map_err(Self::legacy_allocation_error)
    }

    fn try_get_or_create_page_for_write_with<F>(
        &self,
        page_index: usize,
        owner: &Arc<MemoryAccount>,
        creator: &Arc<ObjectAccount>,
        fill: F,
    ) -> Result<Arc<Page>, MemoryAllocationError>
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
                        Err(e) => return Err(e.into()),
                    },
                    PageState::UpToDate | PageState::Dirty => unreachable!(),
                }
            }

            let page = self.allocate_page_with_owner(
                page_cache_ref.expect("page_cache_ref should exist"),
                page_index,
                owner,
            )?;
            let entry = self.prepare_entry(&page, PageState::Loading, creator)?;
            let (_, inserted) = self.publish_prepared_entry(page_index, &entry, creator)?;
            if !inserted {
                continue;
            }
            self.reconcile_entry_unevictable_for_insert(&entry);

            let populate_result = {
                let mut tmp = [0u8; MMArch::PAGE_SIZE];
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
                    return Err(e.into());
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

    /// Complete every fallible PageCache precondition for a remote write and
    /// pin the exact entry until the caller finishes copying.
    ///
    /// `Ok(None)` means the managed page is no longer this mapping's cache
    /// entry (for example, a private COW page), so only the Page itself should
    /// be dirtied.
    pub(crate) fn prepare_remote_page_dirty(
        self: &Arc<Self>,
        page_index: usize,
        expected_page: &Arc<Page>,
    ) -> Result<Option<PreparedRemotePageDirty>, SystemError> {
        let mut inner = self.inner.lock();
        let Some(entry) = inner.get_entry(page_index) else {
            return Ok(None);
        };
        if !Arc::ptr_eq(&entry.page, expected_page) || !entry.state().is_ready() {
            return Ok(None);
        }
        let next_preparations = inner
            .dirty_preparations
            .checked_add(1)
            .ok_or(SystemError::EOVERFLOW)?;
        self.ensure_dirty_retention_locked(&mut inner)?;
        inner.dirty_preparations = next_preparations;
        let pin = entry.pin();
        Ok(Some(PreparedRemotePageDirty {
            cache: self.clone(),
            entry,
            _pin: pin,
            page_index,
            reservation: PageDirtyReservation {
                cache: self.manager.owner.clone(),
                active: true,
            },
        }))
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
        dirty_pages: &mut CacheIndex,
        page_index: usize,
        entry: &Arc<PageEntry>,
    ) -> PageCacheDirtyPublication {
        let old_state = entry.state();
        let transition = entry.begin_front_dirty_transition(page_index, old_state);
        InnerPageCache::update_index(dirty_pages, entry, 1, page_index, true);
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
        // Reclaim enters with the page write-locked, so it cannot sleep
        // behind an invalidation writer which may need that same page. This
        // read guard also closes frozen writeback's frontier-snapshot -> tag-
        // publication window: reclaim either publishes before the frontier
        // is sampled or leaves the page Dirty for the range owner.
        let Some(_invalidate) = self.try_invalidate_read() else {
            return false;
        };
        let mut guard = self.inner.lock();
        if let Some(entry) = guard.get_entry(page_index) {
            if entry.page.phys_address() != expected_paddr
                || entry.writeback_tag() != 0
                || matches!(
                    entry.state(),
                    PageState::Loading | PageState::Writeback | PageState::Error
                )
            {
                return false;
            }
            let Ok(writeback_incarnation) = guard.allocate_writeback_incarnation() else {
                // Zero is reserved and identities must never wrap: leave the
                // page untouched so a bounded caller can report/retry the
                // exhausted mapping instead of publishing an ABA identity.
                return false;
            };
            let old_state = entry.state();
            entry.account_state_transition(old_state, PageState::Writeback);
            entry
                .writeback_incarnation
                .store(writeback_incarnation, Ordering::Release);
            entry.set_state(PageState::Writeback);
            guard.set_index(page_index, 1, false);
            guard.set_index(page_index, 2, true);
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
            guard.set_index(page_index, 1, false);
            guard.set_index(page_index, 2, false);
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
            guard.set_index(page_index, 1, true);
            guard.set_index(page_index, 2, false);
            entry.set_state(PageState::Dirty);
            entry.wait_queue.wake_all();
        }
    }

    /// Atomically adopt a registered batch of zeroed, intrinsically
    /// unevictable Normal pages.
    ///
    /// This narrow API is for private in-kernel ring backings created with
    /// `PageCache::new_unowned(None, None)`. Caches with an accounting backend are
    /// rejected rather than exposing a partially reservable quota protocol.
    /// Until the final commit, `ManagedPageBatch` rolls PageManager membership
    /// back by identity on every error path.
    pub fn adopt_preallocated_unevictable_batch(
        &self,
        start_page_index: usize,
        batch: ManagedPageBatch,
    ) -> Result<(), SystemError> {
        let creator =
            crate::cgroup::memory::object_for_task(&crate::process::ProcessManager::current_pcb());
        let end_page_index = start_page_index
            .checked_add(batch.pages().len())
            .ok_or(SystemError::EOVERFLOW)?;
        let mut entries = Vec::new();
        entries
            .try_reserve_exact(batch.pages().len())
            .map_err(|_| SystemError::ENOMEM)?;
        for page in batch.pages() {
            let page_guard = page.read();
            if !page_guard.flags().contains(PageFlags::PG_UNEVICTABLE)
                || !page_guard.intrinsic_unevictable()
                || !matches!(page_guard.page_type(), PageType::Normal)
            {
                return Err(SystemError::EINVAL);
            }
            drop(page_guard);
            // ManagedPageBatch is the rollback authority until commit; entry
            // preparation must not independently detach its registered frames.
            let entry = PageEntry::try_new(page.clone(), PageState::UpToDate, &creator)
                .map_err(Self::legacy_allocation_error)?;
            entries.push(Arc::try_new(entry).map_err(|_| SystemError::ENOMEM)?);
        }
        loop {
            let mut prepared_capacity = self
                .prepare_table_capacity(entries.len(), &creator)
                .map_err(Self::legacy_allocation_error)?;
            let reclassify = self.reclassify_lock.lock();
            let mut guard = self.inner.lock();
            if guard.accounting_backend.is_some() {
                return Err(SystemError::EINVAL);
            }
            if (start_page_index..end_page_index).any(|index| guard.pages.contains_key(&index)) {
                return Err(SystemError::EEXIST);
            }
            let retired = match guard.install_capacity(&mut prepared_capacity, entries.len()) {
                Ok(retired) => retired,
                Err(()) => {
                    drop(guard);
                    drop(reclassify);
                    drop(prepared_capacity);
                    continue;
                }
            };
            for (index, entry) in (start_page_index..end_page_index).zip(&entries) {
                entry.account_insert(guard.kind, true);
                guard.pages.insert(index, entry.clone());
            }
            drop(guard);
            drop(reclassify);
            drop(retired);
            break;
        }
        batch.commit();
        Ok(())
    }

    /// Insert a pre-allocated page into page cache and mark it ready.
    /// This is for special in-kernel users (e.g. perf ring buffers).
    pub fn insert_preallocated_unevictable_page(
        &self,
        page_index: usize,
        page: Arc<Page>,
    ) -> Result<(), SystemError> {
        let creator =
            crate::cgroup::memory::object_for_task(&crate::process::ProcessManager::current_pcb());
        let page_guard = page.read();
        if !page_guard.flags().contains(PageFlags::PG_UNEVICTABLE)
            || !matches!(page_guard.page_type(), PageType::Normal)
        {
            return Err(SystemError::EINVAL);
        }
        drop(page_guard);
        let prepared = PageEntry::try_new(page.clone(), PageState::UpToDate, &creator)
            .map_err(Self::legacy_allocation_error)?;
        let entry = Arc::try_new(prepared).map_err(|_| SystemError::ENOMEM)?;
        loop {
            let mut prepared_capacity = self
                .prepare_table_capacity(1, &creator)
                .map_err(Self::legacy_allocation_error)?;
            let reclassify = self.reclassify_lock.lock();
            let mut guard = self.inner.lock();
            if guard.get_entry(page_index).is_some() {
                return Err(SystemError::EEXIST);
            }
            let retired = match guard.install_capacity(&mut prepared_capacity, 1) {
                Ok(retired) => retired,
                Err(()) => {
                    drop(guard);
                    drop(reclassify);
                    drop(prepared_capacity);
                    continue;
                }
            };
            let result = guard.insert_preallocated_entry(page_index, entry.clone());
            drop(guard);
            drop(reclassify);
            drop(retired);
            return result;
        }
    }

    pub fn read_pages(&self, start_page_index: usize, page_num: usize) -> Result<(), SystemError> {
        let task = crate::process::ProcessManager::current_pcb();
        let owner = crate::cgroup::memory::account_for_task(&task);
        let creator = crate::cgroup::memory::object_for_task(&task);
        self.try_read_pages(start_page_index, page_num, &owner, &creator)
            .map_err(Self::legacy_allocation_error)
    }

    pub(crate) fn try_read_pages(
        &self,
        start_page_index: usize,
        page_num: usize,
        owner: &Arc<MemoryAccount>,
        creator: &Arc<ObjectAccount>,
    ) -> Result<(), MemoryAllocationError> {
        if page_num == 0 {
            return Ok(());
        }
        start_page_index
            .checked_add(page_num)
            .ok_or(SystemError::EOVERFLOW)?;

        let cache = self.manager.upgrade()?;
        let Some(backend) = cache.backend() else {
            for offset in 0..page_num {
                cache.try_start_async_read(start_page_index + offset, owner, creator)?;
            }
            return Ok(());
        };

        // Size writers for a batch-capable backend must take
        // invalidate_write. This keeps the preflight EOF stable through every
        // Loading reservation and synchronous submit below.
        let _invalidate = cache.invalidate_read();
        let valid_bytes = backend.read_window_limit(start_page_index, page_num)?;
        if valid_bytes == 0 {
            return Ok(());
        }
        let clipped_pages = valid_bytes.div_ceil(MMArch::PAGE_SIZE).min(page_num);
        let max_batch_pages = backend.read_batch_pages().max(1);
        let end = start_page_index + clipped_pages;
        let mut cursor = start_page_index;
        while cursor < end {
            let run = {
                let inner = cache.inner.lock();
                while cursor < end && inner.get_entry(cursor).is_some() {
                    cursor += 1;
                }
                let run_start = cursor;
                while cursor < end
                    && cursor - run_start < max_batch_pages
                    && inner.get_entry(cursor).is_none()
                {
                    cursor += 1;
                }
                (run_start, cursor - run_start)
            };
            if run.1 == 0 {
                continue;
            }

            let domain_io = cache.try_acquire_domain_io()?;
            let completion = match PageCacheReadBatchCompletion::try_reserve(
                &cache, run.0, run.1, domain_io, owner, creator,
            ) {
                Ok(completion) => completion,
                // A racing reader installed one of these pages after the
                // scan. It owns the fill; a later cache read will wait on it.
                Err(MemoryAllocationError::System(SystemError::EEXIST)) => {
                    cursor = run.0;
                    continue;
                }
                Err(error) => return Err(error),
            };
            let relative_start = (run.0 - start_page_index) * MMArch::PAGE_SIZE;
            let run_capacity = run.1 * MMArch::PAGE_SIZE;
            let run_valid = valid_bytes.saturating_sub(relative_start).min(run_capacity);
            backend.clone().submit_read_batch(
                PageCacheReadBatchRequest {
                    start_index: run.0,
                    page_count: run.1,
                    valid_bytes: run_valid,
                },
                completion,
            );
        }
        Ok(())
    }

    /// 两阶段读取：持锁收集拷贝项，解锁后拷贝到目标缓冲区，避免用户缺页导致自锁
    pub fn read(&self, offset: usize, buf: &mut [u8]) -> Result<usize, SystemError> {
        let task = crate::process::ProcessManager::current_pcb();
        let owner = crate::cgroup::memory::account_for_task(&task);
        let creator = crate::cgroup::memory::object_for_task(&task);
        self.try_read(offset, buf, &owner, &creator)
            .map_err(Self::legacy_allocation_error)
    }

    pub(crate) fn try_read(
        &self,
        offset: usize,
        buf: &mut [u8],
        owner: &Arc<MemoryAccount>,
        creator: &Arc<ObjectAccount>,
    ) -> Result<usize, MemoryAllocationError> {
        // Serialize cache lookup/fill with mapping removal, but release this
        // guard before touching a user destination: its fault may need this
        // same mapping while an invalidator is waiting for our entry pins.
        let invalidate = self.invalidate_read();
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
        copies
            .try_reserve_exact(end_page_index - start_page_index + 1)
            .map_err(|_| SystemError::ENOMEM)?;
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

            let (entry, pin, _) =
                self.try_get_or_create_entry_pinned(page_index, true, owner, creator)?;
            copies.push(CopyItem {
                entry,
                _pin: pin,
                page_index,
                page_offset: read_start - page_start,
                sub_len: page_read_len,
                created: false,
            });
            ret += page_read_len;
        }

        drop(invalidate);
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

    /// Prepare one bounded, ordered cache window without touching userspace.
    /// Allocation and backend failures retain their typed recovery information.
    pub(crate) fn prepare_read_user_batch(
        &self,
        offset: usize,
        len: usize,
        owner: &Arc<MemoryAccount>,
        creator: &Arc<ObjectAccount>,
    ) -> Result<ReadUserBatch, MemoryAllocationError> {
        let mut wait_first = false;
        loop {
            let invalidate = self.invalidate_read();
            let inode = self
                .inode()
                .and_then(|inode| inode.upgrade())
                .ok_or(SystemError::EIO)?;
            let file_size = inode.metadata()?.size.max(0) as usize;
            let page_offset = offset & MMArch::PAGE_OFFSET_MASK;
            let len = len
                .min(file_size.saturating_sub(offset))
                .min(READ_USER_BATCH_PAGES * MMArch::PAGE_SIZE - page_offset);
            let mut batch = ReadUserBatch::default();
            if len == 0 {
                return Ok(batch);
            }
            let first_index = offset >> MMArch::PAGE_SHIFT;
            // After submitting a cold window, wait for its first page while
            // excluding invalidation. Its temporary pin also prevents reclaim
            // from removing it before the single-lock collection below.
            let first = if wait_first {
                Some(self.try_get_or_create_entry_pinned(first_index, true, owner, creator)?)
            } else {
                None
            };
            {
                let inner = self.inner.lock();
                for (slot, item) in batch.items.iter_mut().enumerate() {
                    let page_index = first_index + slot;
                    let Some(entry) = inner.get_entry(page_index) else {
                        break;
                    };
                    if !entry.state().is_ready() {
                        break;
                    }
                    let within_page = if slot == 0 { page_offset } else { 0 };
                    let sub_len = (MMArch::PAGE_SIZE - within_page).min(len - batch.len);
                    let pin = entry.pin();
                    *item = Some(CopyItem {
                        entry,
                        _pin: pin,
                        page_index,
                        page_offset: within_page,
                        sub_len,
                        created: false,
                    });
                    batch.len += sub_len;
                    if batch.len == len {
                        break;
                    }
                }
            }
            drop(first);
            drop(invalidate);
            if batch.len != 0 {
                return Ok(batch);
            }
            // No pins or mapping guards cross the backend/recovery boundary.
            // Existing backend batching supplies the cold window; a ready
            // prefix is delivered immediately instead of waiting for its tail.
            self.try_read_pages(
                first_index,
                (page_offset + len).div_ceil(MMArch::PAGE_SIZE),
                owner,
                creator,
            )?;
            wait_first = true;
        }
    }

    pub fn write(&self, offset: usize, buf: &[u8]) -> Result<usize, SystemError> {
        let task = crate::process::ProcessManager::current_pcb();
        let owner = crate::cgroup::memory::account_for_task(&task);
        let creator = crate::cgroup::memory::object_for_task(&task);
        self.try_write(offset, buf, &owner, &creator)
            .map_err(Self::legacy_allocation_error)
    }

    pub(crate) fn try_write(
        &self,
        offset: usize,
        buf: &[u8],
        owner: &Arc<MemoryAccount>,
        creator: &Arc<ObjectAccount>,
    ) -> Result<usize, MemoryAllocationError> {
        let (mut copies, ret) = self.try_prepare_write_copies(offset, buf.len(), owner, creator)?;
        let mut dirty_reservation = if ret != 0 {
            Some(self.prepare_page_dirty()?)
        } else {
            None
        };
        let mut src_offset = 0;
        for item in &copies.items {
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
        copies.rollback = false;
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
        let item = copies.items.first().ok_or(SystemError::EIO)?;
        debug_assert_eq!(copies.items.len(), 1);
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
        copies.rollback = false;
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
        let task = crate::process::ProcessManager::current_pcb();
        let owner = crate::cgroup::memory::account_for_task(&task);
        let creator = crate::cgroup::memory::object_for_task(&task);
        self.try_write_single_page_segment_with_transition(
            offset,
            buf,
            expected,
            &owner,
            &creator,
            on_published,
        )
        .map_err(Self::legacy_allocation_error)
    }

    pub(crate) fn try_write_single_page_segment_with_transition<G>(
        &self,
        offset: usize,
        buf: &[u8],
        expected: PageCacheExpectedDirtyTransition,
        owner: &Arc<MemoryAccount>,
        creator: &Arc<ObjectAccount>,
        on_published: G,
    ) -> Result<usize, MemoryAllocationError>
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
            return Err(SystemError::EINVAL.into());
        }

        let (mut copies, ret) = self.try_prepare_write_copies(offset, buf.len(), owner, creator)?;
        let item = copies.items.first().ok_or(SystemError::EIO)?;
        if copies.items.len() != 1
            || ret != buf.len()
            || item.page_index != page_index
            || item.page_offset != page_offset
            || item.sub_len != buf.len()
        {
            return Err(SystemError::EIO.into());
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
            return Err(SystemError::ESTALE.into());
        }

        match expected {
            PageCacheExpectedDirtyTransition::Start => {
                if inner.dirty_pages.contains(&item.page_index)
                    || matches!(current.state(), PageState::Dirty | PageState::Writeback)
                {
                    return Err(SystemError::EAGAIN_OR_EWOULDBLOCK.into());
                }
            }
            PageCacheExpectedDirtyTransition::Merge(certificate) => {
                if current.state() != PageState::Dirty
                    || !inner.dirty_pages.contains(&item.page_index)
                    || current.current_dirty_certificate(self.instance_id, item.page_index)?
                        != certificate
                {
                    return Err(SystemError::EAGAIN_OR_EWOULDBLOCK.into());
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
        copies.rollback = false;
        Ok(ret)
    }

    fn prepare_write_copies(
        &self,
        offset: usize,
        len: usize,
    ) -> Result<(PreparedWriteCopies<'_>, usize), SystemError> {
        let task = crate::process::ProcessManager::current_pcb();
        let owner = crate::cgroup::memory::account_for_task(&task);
        let creator = crate::cgroup::memory::object_for_task(&task);
        self.try_prepare_write_copies(offset, len, &owner, &creator)
            .map_err(Self::legacy_allocation_error)
    }

    fn try_prepare_write_copies(
        &self,
        offset: usize,
        len: usize,
        owner: &Arc<MemoryAccount>,
        creator: &Arc<ObjectAccount>,
    ) -> Result<(PreparedWriteCopies<'_>, usize), MemoryAllocationError> {
        let mut copies = PreparedWriteCopies {
            cache: self,
            items: Vec::new(),
            rollback: true,
        };
        if len == 0 {
            return Ok((copies, 0));
        }
        let end = offset.checked_add(len).ok_or(SystemError::EFBIG)?;
        let start_page_index = offset >> MMArch::PAGE_SHIFT;
        let end_page_index = (end - 1) >> MMArch::PAGE_SHIFT;
        copies
            .items
            .try_reserve_exact(end_page_index - start_page_index + 1)
            .map_err(|_| SystemError::ENOMEM)?;

        for page_index in start_page_index..=end_page_index {
            let page_start = page_index * MMArch::PAGE_SIZE;
            let write_start = core::cmp::max(offset, page_start);
            let write_end = core::cmp::min(end, page_start.saturating_add(MMArch::PAGE_SIZE));
            let page_write_len = write_end - write_start;
            let full_page_overwrite =
                write_start == page_start && page_write_len == MMArch::PAGE_SIZE;
            let populate_backend = !self.is_shmem() && !full_page_overwrite;
            self.discard_error_entry(page_index);
            let (entry, pin, created) =
                self.try_get_or_create_entry_pinned(page_index, populate_backend, owner, creator)?;
            copies.items.push(CopyItem {
                entry,
                _pin: pin,
                page_index,
                page_offset: write_start - page_start,
                sub_len: page_write_len,
                created,
            });
        }
        Ok((copies, len))
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
        let task = crate::process::ProcessManager::current_pcb();
        let owner = crate::cgroup::memory::account_for_task(&task);
        let creator = crate::cgroup::memory::object_for_task(&task);
        self.try_write_with_before_dirty(offset, buf, &owner, &creator, before_dirty)
            .map_err(Self::legacy_allocation_error)
    }

    pub(crate) fn try_write_with_before_dirty<F>(
        &self,
        offset: usize,
        buf: &[u8],
        owner: &Arc<MemoryAccount>,
        creator: &Arc<ObjectAccount>,
        before_dirty: F,
    ) -> Result<usize, MemoryAllocationError>
    where
        F: FnOnce(usize) -> Result<(), SystemError>,
    {
        let (mut copies, ret) = self.try_prepare_write_copies(offset, buf.len(), owner, creator)?;
        if ret == 0 {
            return Ok(0);
        }

        let mut src_offset = 0;
        for item in &copies.items {
            // Prefault each source segment before the metadata commit so the
            // remaining page-locked copy path cannot introduce a new failure
            // point after the filesystem publishes the write.
            let _ = volatile_read!(buf[src_offset]);
            src_offset += item.sub_len;
        }

        // Lock in ascending page-index order (the same order as `copies`) so
        // readers and writeback cannot observe metadata for the new EOF until
        // all copied bytes and PG_DIRTY transitions are ready to be exposed.
        let mut page_guards = Vec::new();
        page_guards
            .try_reserve_exact(copies.items.len())
            .map_err(|_| SystemError::ENOMEM)?;
        page_guards.extend(copies.items.iter().map(|item| item.entry.page.write()));

        let mut dirty_reservation = self.prepare_page_dirty()?;
        before_dirty(ret)?;

        src_offset = 0;
        for (item, page_guard) in copies.items.iter().zip(page_guards.iter_mut()) {
            unsafe {
                page_guard.as_slice_mut()[item.page_offset..item.page_offset + item.sub_len]
                    .copy_from_slice(&buf[src_offset..src_offset + item.sub_len]);
            }
            page_guard.add_flags(PageFlags::PG_DIRTY);
            src_offset += item.sub_len;
        }
        for (index, (item, page_guard)) in copies.items.iter().zip(page_guards.iter()).enumerate() {
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

        copies.rollback = false;
        Ok(ret)
    }
}

impl PageCacheDomainMember for PageCache {
    fn has_dirty_or_writeback_work(&self) -> bool {
        self.has_dirty_or_writeback_pages()
    }

    fn start_sync_for_domain(&self) -> Result<PageCacheDomainWriteback, SystemError> {
        self.manager.start_sync_for_domain()
    }

    fn retire_for_domain(&self) -> Result<(), SystemError> {
        self.manager.resize(0)
    }
}
