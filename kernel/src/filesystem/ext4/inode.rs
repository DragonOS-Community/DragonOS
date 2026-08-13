use crate::{
    arch::{CurrentTimeArch, MMArch},
    driver::base::device::device_number::{DeviceNumber, Major},
    exception::workqueue::{schedule_work, Work},
    filesystem::{
        page_cache::{
            AsyncPageCacheBackend, PageCache, PageCacheBackend, PageCacheDirtyCertificate,
            PageCacheExpectedDirtyTransition, PageCacheWritebackAdmissionOrder,
            PageCacheWritebackBindResult, PageCacheWritebackCancellationContext,
            PageCacheWritebackDescriptor, PageCacheWritebackDispatchOutcome,
            PageCacheWritebackProgress, PageCacheWritebackProgressOutcome,
            PageCacheWritebackProtocol, PageCacheWritebackSnapshotPhase,
            PageCacheWritebackSubmission, PageCacheWritebackSubmitResult,
        },
        vfs::{
            self, syscall::RenameFlags, utils::DName, vcore::generate_inode_id, FilePrivateData,
            IndexNode, InodeFlags, InodeId, InodeMode, InodeRetentionState, SetMetadataMask,
            SpecialNodeData, XattrFlags,
        },
    },
    ipc::pipe::LockedPipeInode,
    libs::{
        casting::DowncastArc,
        mutex::{Mutex, MutexGuard},
        rwsem::{RwSem, RwSemReadGuard},
        spinlock::SpinLock,
        wait_queue::WaitQueue,
    },
    mm::MemoryManagementArch,
    process::{ProcessManager, RawPid},
    time::{PosixTimeSpec, TimeArch},
};
use alloc::{
    boxed::Box,
    collections::BTreeMap,
    format,
    string::String,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::{
    fmt::Debug,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
};
use kdepends::another_ext4::{self, FileType};
use num::ToPrimitive;
use system_error::SystemError;

use super::filesystem::Ext4FileSystem;

const WHITEOUT_DEV: DeviceNumber = DeviceNumber::new(Major::UNNAMED_MAJOR, 0);

bitflags! {
    /// Inode 脏状态标志位，对应 Linux `inode->i_state` 中的 `I_DIRTY_*` 位。
    pub(super) struct InodeDirtyState: u32 {
        /// 文件大小变更未刷盘，对应 I_DIRTY_SYNC (1 << 0)
        const SIZE_DIRTY    = 1 << 0;
        /// mtime 变更未刷盘，对应 I_DIRTY_DATASYNC (1 << 1)
        const MTIME_DIRTY   = 1 << 1;
        /// atime 变更未刷盘。读路径仅更新缓存，由 inode writeback 持久化。
        const ATIME_DIRTY   = 1 << 2;
        /// 该 inode 已在文件系统 dirty_inodes 队列中。
        const QUEUED        = 1 << 3;
        /// 该 inode 正在执行元数据写回。
        const WRITEBACK     = 1 << 4;
        /// ctime 变更未刷盘。它与 mtime 使用独立版本，避免同秒 ABA。
        const CTIME_DIRTY   = 1 << 5;
        /// 需要持久化的缓存元数据集合。
        const PERSISTENT_DIRTY = Self::SIZE_DIRTY.bits()
            | Self::MTIME_DIRTY.bits()
            | Self::ATIME_DIRTY.bits()
            | Self::CTIME_DIRTY.bits();
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Ext4InodeLifecycleState {
    Live,
    Freeing,
    Retired,
    Poisoned(SystemError),
}

#[derive(Debug)]
struct Ext4InodeLifecycleInner {
    state: Ext4InodeLifecycleState,
    active_operations: usize,
    operation_owners: BTreeMap<RawPid, usize>,
}

#[derive(Debug)]
pub(super) struct Ext4InodeLifecycle {
    inner: Mutex<Ext4InodeLifecycleInner>,
    link_mutation: Mutex<()>,
    wait_queue: WaitQueue,
}

impl Ext4InodeLifecycle {
    pub(super) fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(Ext4InodeLifecycleInner {
                state: Ext4InodeLifecycleState::Live,
                active_operations: 0,
                operation_owners: BTreeMap::new(),
            }),
            link_mutation: Mutex::new(()),
            wait_queue: WaitQueue::default(),
        })
    }

    pub(super) fn state(&self) -> Ext4InodeLifecycleState {
        self.inner.lock().state.clone()
    }

    /// Serializes link-count mutations for all aliases of this canonical inode.
    pub(super) fn lock_link_mutation(&self) -> MutexGuard<'_, ()> {
        self.link_mutation.lock()
    }

    pub(super) fn begin_operation(self: &Arc<Self>) -> Result<Ext4InodeOperation, SystemError> {
        let owner = ProcessManager::current_pcb().raw_pid();
        let mut inner = self.inner.lock();
        match inner.state.clone() {
            Ext4InodeLifecycleState::Live => {}
            Ext4InodeLifecycleState::Freeing if inner.operation_owners.contains_key(&owner) => {}
            Ext4InodeLifecycleState::Freeing => return Err(SystemError::EBUSY),
            Ext4InodeLifecycleState::Retired => return Err(SystemError::ESTALE),
            Ext4InodeLifecycleState::Poisoned(error) => return Err(error),
        }

        let active_operations = inner
            .active_operations
            .checked_add(1)
            .ok_or(SystemError::EOVERFLOW)?;
        let owner_depth = inner
            .operation_owners
            .get(&owner)
            .copied()
            .unwrap_or(0)
            .checked_add(1)
            .ok_or(SystemError::EOVERFLOW)?;
        inner.active_operations = active_operations;
        inner.operation_owners.insert(owner, owner_depth);
        Ok(Ext4InodeOperation {
            lifecycle: self.clone(),
            owner,
        })
    }

    pub(super) fn begin_freeing(&self) -> Result<(), SystemError> {
        let mut inner = self.inner.lock();
        match inner.state.clone() {
            Ext4InodeLifecycleState::Live => {
                inner.state = Ext4InodeLifecycleState::Freeing;
                Ok(())
            }
            Ext4InodeLifecycleState::Freeing => Err(SystemError::EBUSY),
            Ext4InodeLifecycleState::Retired => Err(SystemError::ESTALE),
            Ext4InodeLifecycleState::Poisoned(error) => Err(error),
        }
    }

    pub(super) fn wait_for_quiescent(&self) {
        self.wait_queue.wait_until(|| {
            let inner = self.inner.lock();
            (inner.active_operations == 0).then_some(())
        });
    }

    pub(super) fn wait_while_freeing(&self) -> Ext4InodeLifecycleState {
        self.wait_queue.wait_until(|| {
            let state = self.inner.lock().state.clone();
            (state != Ext4InodeLifecycleState::Freeing).then_some(state)
        })
    }

    pub(super) fn set_state(&self, state: Ext4InodeLifecycleState) {
        self.inner.lock().state = state;
        self.wait_queue.wake_all();
    }
}

#[must_use]
#[derive(Debug)]
pub(super) struct Ext4InodeOperation {
    lifecycle: Arc<Ext4InodeLifecycle>,
    owner: RawPid,
}

/// Keeps the ext4 mmap write-preparation critical section alive until the
/// generic page-cache layer has made the page writable and dirty.
pub(super) struct Ext4MmapWriteGuard<'a> {
    _operation: Ext4InodeOperation,
    _size_guard: RwSemReadGuard<'a, ()>,
}

pub(super) struct ProductionDelallocAdmissionGuard<'a> {
    inode: &'a LockedExt4Inode,
}

impl Drop for ProductionDelallocAdmissionGuard<'_> {
    fn drop(&mut self) {
        let _io = self.inode.io_lock.lock();
        let mut guard = self.inode.inner.lock();
        debug_assert!(guard.delalloc.production.admission_closed != 0);
        guard.delalloc.production.admission_closed =
            guard.delalloc.production.admission_closed.saturating_sub(1);
    }
}

impl Drop for Ext4InodeOperation {
    fn drop(&mut self) {
        let should_wake = {
            let mut inner = self.lifecycle.inner.lock();
            debug_assert!(inner.active_operations > 0);
            inner.active_operations = inner.active_operations.saturating_sub(1);
            let remove_owner =
                if let Some(owner_depth) = inner.operation_owners.get_mut(&self.owner) {
                    debug_assert!(*owner_depth > 0);
                    *owner_depth = owner_depth.saturating_sub(1);
                    *owner_depth == 0
                } else {
                    debug_assert!(false, "missing ext4 lifecycle operation owner");
                    false
                };
            if remove_owner {
                inner.operation_owners.remove(&self.owner);
            }
            inner.active_operations == 0
        };
        if should_wake {
            self.lifecycle.wait_queue.wake_all();
        }
    }
}

type PrivateData<'a> = crate::libs::mutex::MutexGuard<'a, vfs::FilePrivateData>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Ext4InodeTimes {
    pub(super) atime: u32,
    pub(super) mtime: u32,
    pub(super) ctime: u32,
}

impl From<&another_ext4::FileAttr> for Ext4InodeTimes {
    fn from(attr: &another_ext4::FileAttr) -> Self {
        Self {
            atime: attr.atime,
            mtime: attr.mtime,
            ctime: attr.ctime,
        }
    }
}

struct Ext4FrozenMetadata {
    fs: Arc<Ext4FileSystem>,
    inode_num: u32,
    dirty: InodeDirtyState,
    cached_size: Option<u64>,
    cached_times: Ext4InodeTimes,
    atime_version: u64,
    mtime_version: u64,
    ctime_version: u64,
}

/// Inode-local state for production delayed-allocation writeback.
#[derive(Debug, Default)]
struct DelallocInodeState {
    production: ProductionDelallocState,
}

#[derive(Debug)]
struct ProductionDelallocState {
    entries: BTreeMap<usize, ProductionDelallocEntry>,
    next_sequence: u64,
    next_claim_incarnation: u64,
    admission_closed: usize,
    queue_operation: Option<Ext4InodeOperation>,
}

impl Default for ProductionDelallocState {
    fn default() -> Self {
        Self {
            entries: BTreeMap::new(),
            next_sequence: 1,
            next_claim_incarnation: 0,
            admission_closed: 0,
            queue_operation: None,
        }
    }
}

#[derive(Debug)]
struct ProductionDelallocEntry {
    sequence: u64,
    state: ProductionDelallocEntryState,
}

#[derive(Debug)]
enum ProductionDelallocEntryState {
    Prepared(ProductionDelallocPending),
    Ready(ProductionDelallocPending),
    Claimed {
        claim_incarnation: u64,
        certificate: PageCacheDirtyCertificate,
        durable_eof: u64,
    },
}

#[derive(Debug)]
struct ProductionDelallocPending {
    reservation: another_ext4::DelallocAppendBlockReservation,
    certificate: Option<PageCacheDirtyCertificate>,
    offset: usize,
    durable_eof: u64,
    mtime: u32,
    ctime: u32,
    mtime_version: u64,
    ctime_version: u64,
}

impl ProductionDelallocState {
    fn head(&self) -> Option<(&usize, &ProductionDelallocEntry)> {
        self.entries.first_key_value()
    }

    fn head_is_claimed(&self) -> bool {
        self.head().is_some_and(|(_, entry)| {
            matches!(entry.state, ProductionDelallocEntryState::Claimed { .. })
        })
    }

    fn ready_prefix_end(&self, first_offset: usize, max_entries: usize) -> Option<usize> {
        if max_entries == 0
            || self
                .head()
                .is_none_or(|(&head_offset, _)| head_offset != first_offset)
        {
            return None;
        }
        let mut expected = first_offset;
        let mut last = None;
        let mut count = 0usize;
        for (&offset, entry) in self.entries.range(first_offset..) {
            if offset != expected || !matches!(entry.state, ProductionDelallocEntryState::Ready(_))
            {
                break;
            }
            last = Some(offset);
            count += 1;
            if count == max_entries {
                break;
            }
            expected = expected.checked_add(MMArch::PAGE_SIZE)?;
        }
        last
    }
}

type DelallocProgressCallback = Arc<dyn Fn(PageCacheWritebackProgressOutcome) + Send + Sync>;

pub(super) struct Ext4DelallocProgress {
    sequence: AtomicU64,
    demand: AtomicBool,
    work_scheduled: AtomicBool,
    terminal_error: Mutex<Option<SystemError>>,
    wait_queue: WaitQueue,
    callbacks: Mutex<Vec<DelallocProgressCallback>>,
}

impl Debug for Ext4DelallocProgress {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter.write_str("Ext4DelallocProgress(..)")
    }
}

impl Ext4DelallocProgress {
    pub(super) fn new() -> Arc<Self> {
        Arc::new(Self {
            sequence: AtomicU64::new(1),
            demand: AtomicBool::new(false),
            work_scheduled: AtomicBool::new(false),
            terminal_error: Mutex::new(None),
            wait_queue: WaitQueue::default(),
            callbacks: Mutex::new(Vec::new()),
        })
    }

    fn ticket(
        self: &Arc<Self>,
        inode: Weak<LockedExt4Inode>,
        page_cache: Weak<PageCache>,
    ) -> Arc<Ext4DelallocProgressTicket> {
        Arc::new(Ext4DelallocProgressTicket {
            progress: self.clone(),
            inode,
            page_cache,
            observed: self.sequence.load(Ordering::Acquire),
        })
    }

    fn publish(&self, outcome: PageCacheWritebackProgressOutcome) {
        if let PageCacheWritebackProgressOutcome::Failed(error) = &outcome {
            *self.terminal_error.lock() = Some(error.clone());
        }
        self.sequence.fetch_add(1, Ordering::AcqRel);
        self.wait_queue.wake_all();
        let callbacks = core::mem::take(&mut *self.callbacks.lock());
        for callback in callbacks {
            callback(outcome.clone());
        }
    }

    fn schedule_if_demanded_admitted(
        progress: &Arc<Self>,
        inode: &Weak<LockedExt4Inode>,
        domain_io: crate::filesystem::page_cache::PageCacheDomainIoPermit,
    ) {
        if !progress.demand.load(Ordering::Acquire)
            || progress
                .work_scheduled
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
        {
            return;
        }

        let Some(inode) = inode.upgrade() else {
            progress.work_scheduled.store(false, Ordering::Release);
            progress.publish(PageCacheWritebackProgressOutcome::Cancelled);
            return;
        };
        let progress = progress.clone();
        let inode_weak = Arc::downgrade(&inode);
        let work_state = Mutex::new(Some((inode, domain_io)));
        schedule_work(Work::new(move || {
            let Some((inode, domain_io)) = work_state.lock().take() else {
                return;
            };
            // This producer is an admitted filesystem-I/O lineage. Keep the
            // permit through state publication, callback execution, and the
            // lost-wakeup handoff below; a dormant callback never owns it.
            // Consume only the demand observed by this invocation. A
            // concurrent arm() sets it again and is handed off below.
            progress.demand.store(false, Ordering::Release);
            let head = {
                let guard = inode.inner.lock();
                guard.delalloc.production.head().map(|(_, head)| {
                    let page_cache = guard.page_cache.clone();
                    match &head.state {
                        ProductionDelallocEntryState::Ready(pending) => {
                            let first_page = pending.offset / MMArch::PAGE_SIZE;
                            let last_page = guard
                                .delalloc
                                .production
                                .ready_prefix_end(pending.offset, usize::MAX)
                                .unwrap_or(pending.offset)
                                / MMArch::PAGE_SIZE;
                            Ext4DelallocProducerAction::Dispatch(
                                page_cache, first_page, last_page,
                            )
                        }
                        ProductionDelallocEntryState::Prepared(_)
                        | ProductionDelallocEntryState::Claimed { .. } => {
                            Ext4DelallocProducerAction::Passive
                        }
                    }
                })
            };
            let outcome = match head {
                Some(Ext4DelallocProducerAction::Dispatch(
                    Some(page_cache),
                    first_page,
                    last_page,
                )) => Ext4DelallocProducerRunOutcome::Dispatch(
                    page_cache
                        .manager()
                        .dispatch_writeback_once(first_page, last_page, &domain_io),
                ),
                Some(Ext4DelallocProducerAction::Dispatch(None, _, _)) | None => {
                    Ext4DelallocProducerRunOutcome::Cancelled
                }
                Some(Ext4DelallocProducerAction::Passive) => {
                    Ext4DelallocProducerRunOutcome::Passive
                }
            };

            // Publish terminal progress only after this producer has released
            // its scheduling ownership and handed off any demand observed
            // during the run. The producer permit remains held through this
            // entire block; dormant callbacks never own a permit.
            if matches!(
                &outcome,
                Ext4DelallocProducerRunOutcome::Dispatch(Ok(
                    PageCacheWritebackDispatchOutcome::Deferred
                ))
            ) {
                progress.demand.store(true, Ordering::Release);
            }
            progress.work_scheduled.store(false, Ordering::Release);
            // Lost-wakeup handoff: arm-before-clear leaves demand set and this
            // side schedules it; arm-after-clear wins the CAS on its own.
            if matches!(
                &outcome,
                Ext4DelallocProducerRunOutcome::Dispatch(Ok(
                    PageCacheWritebackDispatchOutcome::Progress
                        | PageCacheWritebackDispatchOutcome::Deferred
                )) | Ext4DelallocProducerRunOutcome::Passive
            ) {
                Self::schedule_if_demanded_admitted(
                    &progress,
                    &inode_weak,
                    domain_io.derive(),
                );
            }

            match outcome {
                Ext4DelallocProducerRunOutcome::Dispatch(Ok(
                    PageCacheWritebackDispatchOutcome::Progress,
                )) => {
                    // The successful delayed submission publishes the exact
                    // queue transition itself.
                }
                Ext4DelallocProducerRunOutcome::Dispatch(Ok(
                    PageCacheWritebackDispatchOutcome::Deferred,
                )) => {}
                Ext4DelallocProducerRunOutcome::Dispatch(Ok(
                    PageCacheWritebackDispatchOutcome::Idle,
                ))
                | Ext4DelallocProducerRunOutcome::Cancelled => {
                    // Truncate/unlink or a racing consumer removed the head.
                    // This is cancellation/revalidation, not a sticky I/O
                    // failure.
                    progress.publish(PageCacheWritebackProgressOutcome::Cancelled);
                }
                Ext4DelallocProducerRunOutcome::Dispatch(Err(error)) => {
                    progress.publish(PageCacheWritebackProgressOutcome::Failed(error));
                }
                Ext4DelallocProducerRunOutcome::Passive => {}
            }
        }));
    }
}

enum Ext4DelallocProducerAction {
    Dispatch(Option<Arc<PageCache>>, usize, usize),
    Passive,
}

enum Ext4DelallocProducerRunOutcome {
    Dispatch(Result<PageCacheWritebackDispatchOutcome, SystemError>),
    Passive,
    Cancelled,
}

struct Ext4DelallocProgressTicket {
    progress: Arc<Ext4DelallocProgress>,
    inode: Weak<LockedExt4Inode>,
    page_cache: Weak<PageCache>,
    observed: u64,
}

impl Debug for Ext4DelallocProgressTicket {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter.write_str("Ext4DelallocProgressTicket(..)")
    }
}

impl Ext4DelallocProgressTicket {
    fn outcome_if_changed(&self) -> Option<PageCacheWritebackProgressOutcome> {
        if let Some(error) = self.progress.terminal_error.lock().clone() {
            return Some(PageCacheWritebackProgressOutcome::Failed(error));
        }
        (self.progress.sequence.load(Ordering::Acquire) != self.observed)
            .then_some(PageCacheWritebackProgressOutcome::Progress)
    }

    fn wait_for_progress_interruptible(
        &self,
    ) -> Result<PageCacheWritebackProgressOutcome, SystemError> {
        let current = ProcessManager::current_pcb();
        if current.has_pending_signal_fast() && current.has_pending_not_masked_signal() {
            return Err(SystemError::ERESTARTSYS);
        }
        self.progress
            .wait_queue
            .wait_until_interruptible(|| self.outcome_if_changed())
    }
}

impl PageCacheWritebackProgress for Ext4DelallocProgressTicket {
    fn arm(&self) {
        self.progress.demand.store(true, Ordering::Release);
        let Some(page_cache) = self.page_cache.upgrade() else {
            self.progress
                .publish(PageCacheWritebackProgressOutcome::Cancelled);
            return;
        };
        match page_cache.try_acquire_domain_io_classified() {
            Ok(Some(permit)) => Ext4DelallocProgress::schedule_if_demanded_admitted(
                &self.progress,
                &self.inode,
                permit,
            ),
            Ok(None) => self
                .progress
                .publish(PageCacheWritebackProgressOutcome::Cancelled),
            Err(crate::filesystem::page_cache::PageCacheDomainIoAdmissionError::Closed) => self
                .progress
                .publish(PageCacheWritebackProgressOutcome::Cancelled),
            Err(crate::filesystem::page_cache::PageCacheDomainIoAdmissionError::Unavailable(
                error,
            )) => self
                .progress
                .publish(PageCacheWritebackProgressOutcome::Failed(error)),
        }
    }

    fn wait_for_progress(&self) -> PageCacheWritebackProgressOutcome {
        self.progress
            .wait_queue
            .wait_until(|| self.outcome_if_changed())
    }

    fn register_retry(&self, retry: DelallocProgressCallback) {
        let mut callbacks = self.progress.callbacks.lock();
        if let Some(outcome) = self.outcome_if_changed() {
            drop(callbacks);
            retry(outcome);
        } else {
            callbacks.push(retry);
        }
    }
}

pub struct Ext4Inode {
    // 对应another_ext4里面的inode号，用于在ext4文件系统中查找相应的inode
    pub(super) inner_inode_num: u32,
    pub(super) fs_ptr: Weak<super::filesystem::Ext4FileSystem>,
    pub(super) page_cache: Option<Arc<PageCache>>,
    pub(super) children: BTreeMap<DName, Arc<LockedExt4Inode>>,
    pub(super) dname: DName,

    // 对应vfs的inode id，用于标识系统中唯一的inode
    pub(super) vfs_inode_id: InodeId,

    // 指向父级IndexNode的Weak指针
    pub(super) parent: Weak<LockedExt4Inode>,

    // 指向自身的Weak指针，用于获取Arc<Self>
    pub(super) self_ref: Weak<LockedExt4Inode>,

    // 特殊节点数据（用于 FIFO 的 pipe inode）
    pub(super) special_node: Option<SpecialNodeData>,

    /// 缓存的文件大小，避免频繁调用 getattr/setattr。
    /// None 表示未初始化（第一次写时从磁盘读取并缓存）。
    pub(super) cached_file_size: Option<u64>,
    /// Linux inode-style authoritative in-memory timestamps. They are loaded
    /// before the canonical inode is published; atime/mtime writeback is lazy.
    pub(super) cached_times: Ext4InodeTimes,
    /// Monotonic sequence for atime cache mutations. Disk commits compare the
    /// sequence, not only the value, so A->B->A updates cannot be lost.
    pub(super) cached_atime_version: u64,
    /// Monotonic sequence for mtime cache mutations; mmap write preparation
    /// updates mtime after releasing io_lock, so setters/writeback use this
    /// sequence to avoid same-second ABA and lost dirty state.
    pub(super) cached_mtime_version: u64,
    /// Monotonic sequence for ctime cache mutations.
    pub(super) cached_ctime_version: u64,
    /// Highest timestamp versions known to have crossed the lower metadata
    /// commit boundary. Frozen fsync snapshots compare against these
    /// frontiers, never against a newer merely-cached value.
    pub(super) durable_atime_version: u64,
    pub(super) durable_mtime_version: u64,
    pub(super) durable_ctime_version: u64,
    /// Production delayed-allocation reservation and writeback queue.
    delalloc: DelallocInodeState,
    /// 脏状态标志位，对应 Linux `inode->i_state & I_DIRTY_*`。
    pub(super) dirty_state: InodeDirtyState,
}

#[derive(Debug)]
pub struct LockedExt4Inode {
    pub(super) inner: Mutex<Ext4Inode>,
    pub(super) io_lock: Mutex<()>,
    /// Orders timestamp publication by VFS version across delayed mapper
    /// transactions and frozen fsync metadata commits.
    pub(super) metadata_commit_lock: Mutex<()>,
    pub(super) size_lock: RwSem<()>,
    pub(super) namespace_lock: Mutex<()>,
    pub(super) lifecycle: Arc<Ext4InodeLifecycle>,
    pub(super) retention: InodeRetentionState,
    pub(super) pending_reclaim: SpinLock<Option<another_ext4::InodeReclaimHandle>>,
    pub(super) eviction_scheduled: SpinLock<bool>,
    pub(super) retention_callback_self: Weak<LockedExt4Inode>,
    pub(super) eviction_filesystem: SpinLock<Weak<Ext4FileSystem>>,
    pub(super) delalloc_progress: Arc<Ext4DelallocProgress>,
    pub(super) delalloc_pool: Mutex<Option<another_ext4::DelallocExtentNodePool>>,
}

#[derive(Debug)]
struct Ext4PageCacheBackend {
    inode: Weak<LockedExt4Inode>,
}

impl Ext4PageCacheBackend {
    fn new(inode: Weak<LockedExt4Inode>) -> Self {
        Self { inode }
    }

    fn inode(&self) -> Result<Arc<LockedExt4Inode>, SystemError> {
        self.inode.upgrade().ok_or(SystemError::ESTALE)
    }
}

struct Ext4EagerSubmission {
    inode: Arc<LockedExt4Inode>,
    _operation: Ext4InodeOperation,
}

struct Ext4DelayedSubmission {
    inode: Arc<LockedExt4Inode>,
    fs: Arc<Ext4FileSystem>,
    entries: Vec<ClaimedDelallocEntry>,
    claim_incarnation: u64,
    progress: Arc<Ext4DelallocProgress>,
}

struct ClaimedDelallocEntry {
    pending: ProductionDelallocPending,
    sequence: u64,
}

impl Ext4DelayedSubmission {
    fn claimed_entries_match(
        state: &ProductionDelallocState,
        entries: &[ClaimedDelallocEntry],
        claim_incarnation: u64,
    ) -> bool {
        entries.iter().all(|claimed| {
            state
                .entries
                .get(&claimed.pending.offset)
                .is_some_and(|entry| {
                    entry.sequence == claimed.sequence
                        && matches!(
                            entry.state,
                            ProductionDelallocEntryState::Claimed {
                                claim_incarnation: current,
                                certificate,
                                ..
                            } if current == claim_incarnation
                                && Some(certificate) == claimed.pending.certificate
                        )
                })
        })
    }

    fn restore_pending(
        inode: &LockedExt4Inode,
        entries: Vec<ClaimedDelallocEntry>,
        claim_incarnation: u64,
    ) -> core::ops::ControlFlow<Vec<ClaimedDelallocEntry>> {
        let mut guard = inode.inner.lock();
        if !Self::claimed_entries_match(&guard.delalloc.production, &entries, claim_incarnation) {
            return core::ops::ControlFlow::Break(entries);
        }
        for claimed in entries {
            let offset = claimed.pending.offset;
            let removed = guard.delalloc.production.entries.remove(&offset);
            debug_assert!(removed.is_some());
            assert!(guard
                .delalloc
                .production
                .entries
                .insert(
                    offset,
                    ProductionDelallocEntry {
                        sequence: claimed.sequence,
                        state: ProductionDelallocEntryState::Ready(claimed.pending),
                    },
                )
                .is_none());
        }
        core::ops::ControlFlow::Continue(())
    }

    fn restore_ready(&mut self, admission_held: bool) -> Result<(), SystemError> {
        if self.entries.is_empty() {
            return Err(SystemError::EIO);
        }
        let entries = core::mem::take(&mut self.entries);
        let result = if admission_held {
            Self::restore_pending(&self.inode, entries, self.claim_incarnation)
        } else {
            let _io = self.inode.io_lock.lock();
            Self::restore_pending(&self.inode, entries, self.claim_incarnation)
        };
        if let core::ops::ControlFlow::Break(entries) = result {
            // Keep exact capability ownership in this submission so the
            // terminal path can consume it after fail-stop.
            self.entries = entries;
            return Err(SystemError::EIO);
        }
        Ok(())
    }

    fn finish_completed(&mut self) -> Result<(), SystemError> {
        if self.entries.is_empty() {
            return Err(SystemError::EIO);
        }
        let entries = core::mem::take(&mut self.entries);
        let empty_cleanup = {
            let _io = self.inode.io_lock.lock();
            let mut guard = self.inode.inner.lock();
            if !Self::claimed_entries_match(
                &guard.delalloc.production,
                &entries,
                self.claim_incarnation,
            ) {
                drop(guard);
                self.fs.fail_stop_lifecycle();
                return Err(SystemError::EIO);
            }
            for claimed in entries.iter() {
                let removed = guard
                    .delalloc
                    .production
                    .entries
                    .remove(&claimed.pending.offset);
                debug_assert!(removed.is_some());
            }
            if guard.delalloc.production.entries.is_empty() {
                let owner = guard
                    .delalloc
                    .production
                    .queue_operation
                    .take()
                    .ok_or(SystemError::EIO)?;
                Some((guard.inner_inode_num, owner))
            } else {
                None
            }
        };
        if let Some((inode_num, owner)) = empty_cleanup {
            let authority = self
                .fs
                .delalloc_mapper_authority
                .as_ref()
                .ok_or(SystemError::EIO)?;
            self.inode
                .release_empty_delalloc_pool(&self.fs, authority)?;
            self.fs.unregister_delalloc_inode(inode_num);
            drop(owner);
        }
        drop(entries);
        Ok(())
    }

    fn finish_terminal(&mut self) {
        let mut claimed = core::mem::take(&mut self.entries);
        let mut idle = Vec::new();
        let (inode_num, owner) = {
            let _io = self.inode.io_lock.lock();
            let mut guard = self.inode.inner.lock();
            for (_, entry) in core::mem::take(&mut guard.delalloc.production.entries) {
                match entry.state {
                    ProductionDelallocEntryState::Prepared(pending)
                    | ProductionDelallocEntryState::Ready(pending) => idle.push(pending),
                    ProductionDelallocEntryState::Claimed { .. } => {}
                }
            }
            (
                guard.inner_inode_num,
                guard.delalloc.production.queue_operation.take(),
            )
        };
        self.fs.fail_stop_lifecycle();
        if let Some(authority) = self.fs.delalloc_mapper_authority.as_ref() {
            for entry in claimed.iter_mut() {
                let _ = self
                    .fs
                    .fs
                    .terminalize_delalloc_append_block_authorized_after_fail_stop(
                        authority,
                        &mut entry.pending.reservation,
                    );
            }
            for mut pending in idle {
                let _ = self
                    .fs
                    .fs
                    .terminalize_delalloc_append_block_authorized_after_fail_stop(
                        authority,
                        &mut pending.reservation,
                    );
            }
            if let Some(mut pool) = self.inode.delalloc_pool.lock().take() {
                let _ = self
                    .fs
                    .fs
                    .terminalize_delalloc_extent_node_pool_authorized_after_fail_stop(
                        authority, &mut pool,
                    );
            }
        }
        self.fs.unregister_delalloc_inode(inode_num);
        drop(owner);
        drop(claimed);
    }
}

impl PageCacheWritebackSubmission for Ext4EagerSubmission {
    fn submit(
        self: Box<Self>,
        descriptor: &PageCacheWritebackDescriptor,
        data: &[u8],
    ) -> Result<PageCacheWritebackSubmitResult, SystemError> {
        let offset = descriptor
            .first_index()
            .checked_mul(MMArch::PAGE_SIZE)
            .ok_or(SystemError::EOVERFLOW)?;
        let written = self.inode.write_sync(offset, data)?;
        if written != data.len() {
            return Err(SystemError::EIO);
        }
        Ok(PageCacheWritebackSubmitResult::Completed)
    }

    fn cancel(self: Box<Self>, _context: PageCacheWritebackCancellationContext) {}
}

impl PageCacheWritebackSubmission for Ext4DelayedSubmission {
    fn submit(
        mut self: Box<Self>,
        descriptor: &PageCacheWritebackDescriptor,
        data: &[u8],
    ) -> Result<PageCacheWritebackSubmitResult, SystemError> {
        let certificates = descriptor.dirty_certificates();
        let descriptor_pages = descriptor
            .last_index()
            .checked_sub(descriptor.first_index())
            .and_then(|distance| distance.checked_add(1));
        if self.entries.is_empty()
            || descriptor_pages != Some(self.entries.len())
            || certificates.len() != self.entries.len()
            || descriptor.valid_bytes() != data.len()
        {
            self.finish_terminal();
            return Err(SystemError::EIO);
        }
        let Some(authority) = self.fs.delalloc_mapper_authority.as_ref() else {
            self.finish_terminal();
            self.progress
                .publish(PageCacheWritebackProgressOutcome::Failed(
                    SystemError::EROFS,
                ));
            return Err(SystemError::EROFS);
        };
        let outcome =
            (|| -> Result<another_ext4::DelallocAppendBlockSubmitOutcome, SystemError> {
                let mut publications = Vec::new();
                let mut reservations = Vec::new();
                publications
                    .try_reserve_exact(self.entries.len())
                    .map_err(|_| SystemError::ENOMEM)?;
                reservations
                    .try_reserve_exact(self.entries.len())
                    .map_err(|_| SystemError::ENOMEM)?;
                for (index, entry) in self.entries.iter_mut().enumerate() {
                    let expected_page = descriptor
                        .first_index()
                        .checked_add(index)
                        .ok_or(SystemError::EOVERFLOW)?;
                    let visible = entry
                        .pending
                        .durable_eof
                        .checked_sub(entry.pending.offset as u64)
                        .and_then(|visible| usize::try_from(visible).ok())
                        .filter(|visible| *visible != 0 && *visible <= MMArch::PAGE_SIZE)
                        .ok_or(SystemError::EIO)?;
                    let data_start = index
                        .checked_mul(MMArch::PAGE_SIZE)
                        .ok_or(SystemError::EOVERFLOW)?;
                    let data_end = data_start
                        .checked_add(visible)
                        .ok_or(SystemError::EOVERFLOW)?;
                    if entry.pending.offset / MMArch::PAGE_SIZE != expected_page
                        || entry.pending.certificate != Some(certificates[index])
                        || data_end > data.len()
                    {
                        return Err(SystemError::EIO);
                    }
                    publications.push(another_ext4::DelallocAppendBlockPublication {
                        payload: &data[data_start..data_end],
                        durable_eof: entry.pending.durable_eof,
                        mtime: Some(entry.pending.mtime),
                        ctime: Some(entry.pending.ctime),
                    });
                    reservations.push(&mut entry.pending.reservation);
                }

                // All fallible descriptor preparation is complete before this
                // mount-scoped serialization point. The lower transaction
                // gate has filesystem scope; waiting here aligns the upper
                // ownership domain without exposing ext4 policy to PageCache.
                let _delalloc_submit = self.fs.delalloc_submit_lock.lock();
                let _metadata_commit = self.inode.metadata_commit_lock.lock();
                let mut pool_slot = self.inode.delalloc_pool.lock();
                let pool = pool_slot.as_mut().ok_or(SystemError::EIO)?;

                let outcome = loop {
                    let observed = self.fs.fs.metadata_mutation_generation();
                    let outcome = self
                        .fs
                        .fs
                        .submit_delalloc_append_batch_authorized_with_pool(
                            authority,
                            &mut reservations,
                            &publications,
                            pool,
                        );
                    if matches!(
                        outcome,
                        another_ext4::DelallocAppendBlockSubmitOutcome::RetryableNotPublished(
                            another_ext4::ErrCode::EAGAIN
                        )
                    ) {
                        // Preserve the exact claimed pages, reservations and
                        // pool while sleeping on the filesystem-wide owner
                        // which can actually make this transaction runnable.
                        self.fs.wait_metadata_mutation_progress(observed)?;
                        continue;
                    }
                    break outcome;
                };
                if matches!(
                    outcome,
                    another_ext4::DelallocAppendBlockSubmitOutcome::Completed
                ) {
                    let mut guard = self.inode.inner.lock();
                    for entry in self.entries.iter() {
                        guard.durable_mtime_version =
                            guard.durable_mtime_version.max(entry.pending.mtime_version);
                        guard.durable_ctime_version =
                            guard.durable_ctime_version.max(entry.pending.ctime_version);
                    }
                }
                Ok(outcome)
            })();
        let outcome = match outcome {
            Ok(outcome) => outcome,
            Err(SystemError::ENOMEM) => {
                self.restore_ready(false)?;
                self.progress
                    .publish(PageCacheWritebackProgressOutcome::Progress);
                return Err(SystemError::ENOMEM);
            }
            Err(error) => {
                self.finish_terminal();
                self.progress
                    .publish(PageCacheWritebackProgressOutcome::Failed(error.clone()));
                return Err(error);
            }
        };
        match outcome {
            another_ext4::DelallocAppendBlockSubmitOutcome::Completed => {
                self.finish_completed()?;
                self.progress
                    .publish(PageCacheWritebackProgressOutcome::Progress);
                Ok(PageCacheWritebackSubmitResult::Completed)
            }
            another_ext4::DelallocAppendBlockSubmitOutcome::RetryableNotPublished(
                another_ext4::ErrCode::EAGAIN,
            ) => {
                self.restore_ready(false)?;
                let page_cache = self
                    .inode
                    .inner
                    .lock()
                    .page_cache
                    .as_ref()
                    .map(Arc::downgrade)
                    .unwrap_or_default();
                Ok(PageCacheWritebackSubmitResult::Deferred(
                    self.progress
                        .ticket(Arc::downgrade(&self.inode), page_cache),
                ))
            }
            another_ext4::DelallocAppendBlockSubmitOutcome::RetryableNotPublished(error) => {
                self.restore_ready(false)?;
                self.progress
                    .publish(PageCacheWritebackProgressOutcome::Progress);
                Err(another_ext4::Ext4Error::new(error).into())
            }
            another_ext4::DelallocAppendBlockSubmitOutcome::Terminal(_) => {
                self.finish_terminal();
                self.progress
                    .publish(PageCacheWritebackProgressOutcome::Failed(SystemError::EIO));
                Err(SystemError::EIO)
            }
        }
    }

    fn cancel(mut self: Box<Self>, context: PageCacheWritebackCancellationContext) {
        let admission_held = matches!(
            context,
            PageCacheWritebackCancellationContext::BeforeSubmitWithAdmission
        );
        if self.restore_ready(admission_held).is_err() {
            self.finish_terminal();
            self.progress
                .publish(PageCacheWritebackProgressOutcome::Failed(SystemError::EIO));
        } else {
            self.progress
                .publish(PageCacheWritebackProgressOutcome::Progress);
        }
    }
}

impl PageCacheBackend for Ext4PageCacheBackend {
    fn read_page(&self, index: usize, buf: &mut [u8]) -> Result<usize, SystemError> {
        let offset = index
            .checked_mul(MMArch::PAGE_SIZE)
            .ok_or(SystemError::EOVERFLOW)?;
        self.inode()?.read_sync(offset, buf)
    }

    fn write_page(&self, index: usize, buf: &[u8]) -> Result<usize, SystemError> {
        let offset = index
            .checked_mul(MMArch::PAGE_SIZE)
            .ok_or(SystemError::EOVERFLOW)?;
        self.inode()?.write_sync(offset, buf)
    }

    fn npages(&self) -> usize {
        self.inode
            .upgrade()
            .and_then(|inode| inode.inner.lock().cached_file_size)
            .unwrap_or(0)
            .div_ceil(MMArch::PAGE_SIZE as u64) as usize
    }

    fn write_batch_pages(&self) -> Result<usize, SystemError> {
        let inode = self.inode()?;
        let fs = inode.inner.lock().concret_fs();
        let authority = fs
            .delalloc_mapper_authority
            .as_ref()
            .ok_or(SystemError::EROFS)?;
        fs.fs
            .max_delalloc_append_batch_blocks_authorized(authority)
            .map_err(SystemError::from)
    }

    fn write_batch_pages_from(&self, first_index: usize) -> Result<usize, SystemError> {
        let max = self.write_batch_pages()?.min(64);
        let inode = self.inode()?;
        let first_offset = first_index
            .checked_mul(MMArch::PAGE_SIZE)
            .ok_or(SystemError::EOVERFLOW)?;
        let guard = inode.inner.lock();
        let Some((&head_offset, _)) = guard.delalloc.production.head() else {
            return Ok(1);
        };
        if head_offset != first_offset {
            return Ok(1);
        }
        let Some(last_offset) = guard
            .delalloc
            .production
            .ready_prefix_end(first_offset, max)
        else {
            return Ok(1);
        };
        Ok((last_offset - first_offset) / MMArch::PAGE_SIZE + 1)
    }

    fn writeback_admission_order(&self) -> PageCacheWritebackAdmissionOrder {
        PageCacheWritebackAdmissionOrder::InvalidateBeforeAdmission
    }

    fn writeback_snapshot_phase(&self) -> PageCacheWritebackSnapshotPhase {
        PageCacheWritebackSnapshotPhase::AfterAdmission
    }

    fn with_write_admission(
        &self,
        claim: &mut dyn FnMut() -> Result<(), SystemError>,
    ) -> Result<(), SystemError> {
        let inode = self.inode()?;
        let _operation = inode.begin_operation()?;
        let _size = inode.size_lock.read();
        let _io = inode.io_lock.lock();
        claim()
    }

    fn try_with_write_admission(
        &self,
        claim: &mut dyn FnMut() -> Result<(), SystemError>,
    ) -> Result<bool, SystemError> {
        let inode = self.inode()?;
        let _operation = inode.begin_operation()?;
        let Some(_size) = inode.size_lock.try_read() else {
            return Ok(false);
        };
        let Ok(_io) = inode.io_lock.try_lock() else {
            return Ok(false);
        };
        claim()?;
        Ok(true)
    }

    fn stable_writeback_size(&self, _inode: &Arc<dyn IndexNode>) -> Result<usize, SystemError> {
        self.inode()?
            .inner
            .lock()
            .cached_file_size
            .map(|size| size as usize)
            .ok_or(SystemError::EIO)
    }

    fn try_stable_writeback_size(
        &self,
        _inode: &Arc<dyn IndexNode>,
    ) -> Result<Option<usize>, SystemError> {
        let inode = self.inode()?;
        let Ok(guard) = inode.inner.try_lock() else {
            return Ok(None);
        };
        Ok(guard.cached_file_size.map(|size| size as usize))
    }

    fn writeback_submission_protocol(&self) -> PageCacheWritebackProtocol {
        PageCacheWritebackProtocol::Token
    }

    fn bind_writeback_submission(
        &self,
        descriptor: &PageCacheWritebackDescriptor,
    ) -> Result<PageCacheWritebackBindResult, SystemError> {
        let inode = self.inode()?;
        let certificates = descriptor.dirty_certificates();
        if certificates.is_empty() {
            return Err(SystemError::EIO);
        }
        let descriptor_offset = descriptor
            .first_index()
            .checked_mul(MMArch::PAGE_SIZE)
            .ok_or(SystemError::EOVERFLOW)?;
        let descriptor_pages = descriptor
            .last_index()
            .checked_sub(descriptor.first_index())
            .and_then(|distance| distance.checked_add(1))
            .ok_or(SystemError::EOVERFLOW)?;
        if descriptor_pages != certificates.len() {
            return Err(SystemError::EIO);
        }
        let mut claimed = Vec::new();
        claimed
            .try_reserve_exact(descriptor_pages)
            .map_err(|_| SystemError::ENOMEM)?;
        let mut guard = inode.inner.lock();
        let head_matches = guard
            .delalloc
            .production
            .head()
            .is_some_and(|(&offset, _)| offset == descriptor_offset);
        let batch_matches = head_matches
            && certificates.iter().enumerate().all(|(index, certificate)| {
                let Some(relative_offset) = index.checked_mul(MMArch::PAGE_SIZE) else {
                    return false;
                };
                descriptor_offset
                    .checked_add(relative_offset)
                    .and_then(|offset| guard.delalloc.production.entries.get(&offset))
                    .is_some_and(|entry| {
                        matches!(
                            &entry.state,
                            ProductionDelallocEntryState::Ready(pending)
                                if descriptor.first_index().checked_add(index)
                                    == Some(pending.offset / MMArch::PAGE_SIZE)
                                    && pending.certificate == Some(*certificate)
                        )
                    })
            });
        if batch_matches {
            let incarnation = guard
                .delalloc
                .production
                .next_claim_incarnation
                .checked_add(1)
                .ok_or(SystemError::EOVERFLOW)?;
            guard.delalloc.production.next_claim_incarnation = incarnation;
            for (index, certificate) in certificates.iter().copied().enumerate() {
                let relative_offset = index
                    .checked_mul(MMArch::PAGE_SIZE)
                    .ok_or(SystemError::EOVERFLOW)?;
                let offset = descriptor_offset
                    .checked_add(relative_offset)
                    .ok_or(SystemError::EOVERFLOW)?;
                let entry = guard
                    .delalloc
                    .production
                    .entries
                    .remove(&offset)
                    .ok_or(SystemError::EIO)?;
                let ProductionDelallocEntryState::Ready(pending) = entry.state else {
                    unreachable!("validated delayed writeback entry changed under inode lock")
                };
                let durable_eof = pending.durable_eof;
                claimed.push(ClaimedDelallocEntry {
                    pending,
                    sequence: entry.sequence,
                });
                assert!(guard
                    .delalloc
                    .production
                    .entries
                    .insert(
                        offset,
                        ProductionDelallocEntry {
                            sequence: entry.sequence,
                            state: ProductionDelallocEntryState::Claimed {
                                claim_incarnation: incarnation,
                                certificate,
                                durable_eof,
                            },
                        },
                    )
                    .is_none());
            }
            let fs = guard.concret_fs();
            drop(guard);
            let progress = inode.delalloc_progress.clone();
            return Ok(PageCacheWritebackBindResult::Submission(Box::new(
                Ext4DelayedSubmission {
                    inode,
                    fs,
                    entries: claimed,
                    claim_incarnation: incarnation,
                    progress,
                },
            )));
        }

        if let Some(entry) = guard.delalloc.production.entries.get(&descriptor_offset) {
            let matches_certificate = match &entry.state {
                ProductionDelallocEntryState::Ready(pending)
                | ProductionDelallocEntryState::Prepared(pending) => {
                    pending.certificate == Some(certificates[0])
                }
                ProductionDelallocEntryState::Claimed {
                    certificate: claimed,
                    ..
                } => *claimed == certificates[0],
            };
            if !matches_certificate {
                return Err(SystemError::EIO);
            }
            let page_cache = guard
                .page_cache
                .as_ref()
                .map(Arc::downgrade)
                .unwrap_or_default();
            drop(guard);
            return Ok(PageCacheWritebackBindResult::Deferred(
                inode
                    .delalloc_progress
                    .ticket(Arc::downgrade(&inode), page_cache),
            ));
        }
        {
            drop(guard);
            let operation = inode.begin_operation()?;
            Ok(PageCacheWritebackBindResult::Submission(Box::new(
                Ext4EagerSubmission {
                    inode,
                    _operation: operation,
                },
            )))
        }
    }
}

impl IndexNode for LockedExt4Inode {
    fn append_lock_fs(&self) -> Option<Arc<dyn vfs::FileSystem>> {
        Some(self.fs())
    }

    fn retention_state(&self) -> Option<&InodeRetentionState> {
        Some(&self.retention)
    }

    fn on_zero_retention(&self) {
        let inode = self.retention_callback_self.upgrade();
        if let Some(inode) = inode {
            let _ = inode.try_schedule_deferred_eviction();
        }
    }

    fn mmap(&self, _start: usize, _len: usize, _offset: usize) -> Result<(), SystemError> {
        Ok(())
    }

    fn open(
        &self,
        _data: crate::libs::mutex::MutexGuard<vfs::FilePrivateData>,
        _mode: &vfs::file::FileFlags,
    ) -> Result<(), SystemError> {
        Ok(())
    }

    fn create(
        &self,
        name: &str,
        file_type: vfs::FileType,
        mode: vfs::InodeMode,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        let _operation = self.begin_operation()?;
        let _io = self.io_lock.lock();
        let _namespace = self.namespace_lock.lock();
        let parent_metadata = self.metadata()?;
        let init = vfs::permission::child_inode_init(&parent_metadata, file_type, mode);
        let mut guard = self.inner.lock();
        // another_ext4的高4位是文件类型，低12位是权限
        let file_mode = InodeMode::from(file_type).union(init.mode);
        let file_mode = another_ext4::InodeMode::from_bits_truncate(file_mode.bits() as u16);
        let fs = guard.concret_fs();
        let _reuse = fs.begin_allocation()?;
        let ext4 = &fs.fs;
        // Resolve the parent lifetime before publishing the on-disk name so
        // no fallible parent lookup remains after the namespace transaction.
        let self_arc = guard.self_ref.upgrade().ok_or(SystemError::ENOENT)?;

        let attr = if file_type == vfs::FileType::Dir {
            fs.retry_metadata_contention(|| {
                ext4.mkdir_with_owner_and_attr(
                    guard.inner_inode_num,
                    name,
                    file_mode,
                    another_ext4::InodeOwner {
                        uid: init.uid as u32,
                        gid: init.gid as u32,
                    },
                )
            })?
        } else {
            fs.retry_metadata_contention(|| {
                ext4.create_with_owner_and_attr(
                    guard.inner_inode_num,
                    name,
                    file_mode,
                    another_ext4::InodeOwner {
                        uid: init.uid as u32,
                        gid: init.gid as u32,
                    },
                )
            })?
        };

        let dname = DName::from(name);
        let inode = fs.publish_allocated_inode(
            attr,
            dname.clone(),
            Some(Arc::downgrade(&self_arc)),
            &_reuse,
        )?;
        // 更新 children 缓存
        guard.children.insert(dname, inode.clone());
        drop(guard);
        Ok(inode as Arc<dyn IndexNode>)
    }

    fn create_with_data(
        &self,
        name: &str,
        file_type: vfs::FileType,
        mode: InodeMode,
        data: usize,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        if data == 0 {
            return self.create(name, file_type, mode);
        }

        Err(SystemError::ENOSYS)
    }

    fn symlink(&self, name: &str, target: &str) -> Result<Arc<dyn IndexNode>, SystemError> {
        let _operation = self.begin_operation()?;
        let _io = self.io_lock.lock();
        let _namespace = self.namespace_lock.lock();
        let parent_metadata = self.metadata()?;
        let init = vfs::permission::child_inode_init(
            &parent_metadata,
            vfs::FileType::SymLink,
            InodeMode::S_IRWXUGO,
        );
        let mut guard = self.inner.lock();
        let fs = guard.concret_fs();
        let _reuse = fs.begin_allocation()?;
        let ext4 = &fs.fs;
        let self_arc = guard.self_ref.upgrade().ok_or(SystemError::ENOENT)?;

        let attr = fs.retry_metadata_contention(|| {
            ext4.symlink_with_owner_and_attr(
                guard.inner_inode_num,
                name,
                target,
                another_ext4::InodeOwner {
                    uid: init.uid as u32,
                    gid: init.gid as u32,
                },
            )
        })?;

        let dname = DName::from(name);
        let inode = fs.publish_allocated_inode(
            attr,
            dname.clone(),
            Some(Arc::downgrade(&self_arc)),
            &_reuse,
        )?;
        guard.children.insert(dname, inode.clone());
        drop(guard);
        Ok(inode as Arc<dyn IndexNode>)
    }

    fn read_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        data: PrivateData,
    ) -> Result<usize, SystemError> {
        let _operation = self.begin_operation()?;
        let len = core::cmp::min(len, buf.len());
        let buf = &mut buf[0..len];

        // 关键修复：不要在持有 Ext4 inode 自旋锁期间调用 PageCache::{read,write}。
        // PageCache 读写路径内部会调用 inode.metadata() 获取文件大小：
        // - prepare_read(): inode.metadata()
        // 若此处持有 inode 锁，则会在 metadata() 再次尝试获取同一把锁而自旋死锁。
        let page_cache = {
            let guard = self.inner.lock();
            guard.page_cache.clone()
        };

        if let Some(page_cache) = page_cache {
            // 性能优化：不再每次 read 都同步更新 atime 到磁盘。
            // 这等同于 Linux 的 noatime 挂载选项，避免每次读取引发
            // read_inode + write_inode 的额外磁盘 I/O。
            page_cache.read(offset, buf)
        } else {
            self.read_direct(offset, len, buf, data)
        }
    }

    fn read_sync(&self, offset: usize, buf: &mut [u8]) -> Result<usize, SystemError> {
        let _operation = self.begin_operation()?;
        let (fs, inode_num) = {
            let guard = self.inner.lock();
            (guard.concret_fs(), guard.inner_inode_num)
        };
        let file_type = fs.fs.getattr(inode_num)?.ftype;
        match file_type {
            FileType::Directory => Err(SystemError::EISDIR),
            FileType::Unknown => Err(SystemError::EROFS),
            FileType::RegularFile => fs.fs.read(inode_num, offset, buf).map_err(Into::into),
            FileType::SymLink => fs.fs.readlink(inode_num, offset, buf).map_err(Into::into),
            _ => Err(SystemError::EINVAL),
        }
    }

    fn read_direct(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        _data: crate::libs::mutex::MutexGuard<vfs::FilePrivateData>,
    ) -> Result<usize, SystemError> {
        let len = core::cmp::min(len, buf.len());
        let _delalloc_admission = self.close_production_delalloc_admission()?;
        self.drain_delalloc_range_before_eager(offset, len)?;
        self.read_sync(offset, &mut buf[0..len])
    }

    fn write_at(
        &self,
        offset: usize,
        len: usize,
        buf: &[u8],
        data: PrivateData,
    ) -> Result<usize, SystemError> {
        let _operation = self.begin_operation()?;
        let len = core::cmp::min(len, buf.len());
        if len == 0 {
            return Ok(0);
        }
        let buf = &buf[0..len];

        let (fs, inode_num, page_cache) = {
            let guard = self.inner.lock();
            (
                guard.concret_fs(),
                guard.inner_inode_num,
                guard.page_cache.clone(),
            )
        };

        if let Some(page_cache) = page_cache {
            let mut delayed_written = 0usize;
            while delayed_written < buf.len() {
                let segment_offset = offset
                    .checked_add(delayed_written)
                    .ok_or(SystemError::EFBIG)?;
                let page_remaining = MMArch::PAGE_SIZE - (segment_offset & (MMArch::PAGE_SIZE - 1));
                let segment_len = core::cmp::min(page_remaining, buf.len() - delayed_written);
                let merged = match self.try_merge_delalloc_tail_segment(
                    &page_cache,
                    segment_offset,
                    &buf[delayed_written..delayed_written + segment_len],
                ) {
                    Ok(result) => result,
                    Err(_) if delayed_written != 0 => return Ok(delayed_written),
                    Err(error) => return Err(error),
                };
                if let Some(written) = merged {
                    delayed_written = delayed_written
                        .checked_add(written)
                        .ok_or(SystemError::EOVERFLOW)?;
                    continue;
                }
                let admitted = match self.try_write_delalloc_new_page_segment(
                    &page_cache,
                    segment_offset,
                    &buf[delayed_written..delayed_written + segment_len],
                ) {
                    Ok(result) => result,
                    Err(_) if delayed_written != 0 => return Ok(delayed_written),
                    Err(error) => return Err(error),
                };
                if let Some(written) = admitted {
                    delayed_written = delayed_written
                        .checked_add(written)
                        .ok_or(SystemError::EOVERFLOW)?;
                    continue;
                }
                if delayed_written != 0 {
                    return Ok(delayed_written);
                }
                let has_head = !self.inner.lock().delalloc.production.entries.is_empty();
                if !has_head {
                    break;
                }
                self.drain_delalloc_before_eager()?;
                break;
            }
            if delayed_written != 0 {
                return Ok(delayed_written);
            }
            let _invalidate = page_cache.invalidate_write();
            let _size_guard = self.size_lock.read();
            let _io_guard = self.io_lock.lock();

            // 使用缓存的文件大小，避免 getattr 磁盘 I/O
            let old_file_size = {
                let cached_size = self.inner.lock().cached_file_size;
                match cached_size {
                    Some(size) => size,
                    None => {
                        let size = fs.fs.getattr(inode_num)?.size;
                        self.inner.lock().cached_file_size = Some(size);
                        size
                    }
                }
            };

            let new_end = offset.checked_add(len).ok_or(SystemError::EFBIG)?;
            let alloc_start = (offset >> MMArch::PAGE_SHIFT) << MMArch::PAGE_SHIFT;
            let alloc_end = new_end
                .checked_add(MMArch::PAGE_SIZE - 1)
                .ok_or(SystemError::EFBIG)?
                & !(MMArch::PAGE_SIZE - 1);
            let alloc_len = alloc_end
                .checked_sub(alloc_start)
                .ok_or(SystemError::EFBIG)?;

            let time = PosixTimeSpec::now().tv_sec.to_u32().unwrap_or_else(|| {
                log::warn!("Failed to get current time, using 0");
                0
            });
            // `io_lock` serializes every mtime/ctime publisher in this inode.
            // Exhaustion must be rejected before lower metadata or PageCache
            // data becomes visible; a post-publication EOVERFLOW cannot be
            // rolled back as a normal buffered-write error.
            let (mtime_version, ctime_version) = {
                let guard = self.inner.lock();
                (
                    guard
                        .cached_mtime_version
                        .checked_add(1)
                        .ok_or(SystemError::EOVERFLOW)?,
                    guard
                        .cached_ctime_version
                        .checked_add(1)
                        .ok_or(SystemError::EOVERFLOW)?,
                )
            };
            let stats_start = fs
                .fs
                .prepare_stats_enabled()
                .then(CurrentTimeArch::get_cycles);
            let prepare_result = fs.retry_metadata_contention(|| {
                fs.fs.prepare_buffered_write(
                    inode_num,
                    alloc_start,
                    alloc_len,
                    new_end as u64,
                    Some(time),
                )
            });
            if let Some(start) = stats_start {
                fs.fs.record_prepare_elapsed_cycles(
                    CurrentTimeArch::get_cycles().wrapping_sub(start),
                );
            }
            prepare_result?;

            // 写入范围的磁盘块已就绪，现在安全写入 page cache。
            let write_len = PageCache::write(&page_cache, offset, buf)?;
            if write_len > 0 {
                let written_end = offset.checked_add(write_len).ok_or(SystemError::EFBIG)?;
                let current_file_size = core::cmp::max(old_file_size, written_end as u64);
                let self_arc = {
                    let mut guard = self.inner.lock();
                    guard.cached_file_size = Some(current_file_size);
                    guard.cached_times.mtime = time;
                    guard.cached_times.ctime = time;
                    guard.cached_mtime_version = mtime_version;
                    guard.cached_ctime_version = ctime_version;
                    guard.self_ref.upgrade().ok_or(SystemError::ENOENT)?
                };
                Ext4FileSystem::mark_inode_dirty(
                    &self_arc,
                    InodeDirtyState::SIZE_DIRTY
                        | InodeDirtyState::MTIME_DIRTY
                        | InodeDirtyState::CTIME_DIRTY,
                )?;
            }

            Ok(write_len)
        } else {
            let _size_guard = self.size_lock.read();
            self.write_direct(offset, len, buf, data)
        }
    }

    fn write_sync(&self, offset: usize, buf: &[u8]) -> Result<usize, SystemError> {
        let _operation = self.begin_operation()?;
        let _delalloc_admission = self.close_production_delalloc_admission()?;
        self.drain_delalloc_before_eager()?;
        let _io_guard = self.io_lock.lock();
        let _metadata_commit = self.metadata_commit_lock.lock();
        let (fs, inode_num) = {
            let guard = self.inner.lock();
            (guard.concret_fs(), guard.inner_inode_num)
        };
        let file_type = fs.fs.getattr(inode_num)?.ftype;
        match file_type {
            FileType::Directory => Err(SystemError::EISDIR),
            FileType::Unknown => Err(SystemError::EROFS),
            // Use write_data_only: blocks are pre-allocated by prepare_buffered_write() in write_at().
            // Using Ext4::write() here would cause it to call write_inode_with_csum()
            // which overwrites the inode's block_count/extent tree with a stale
            // snapshot, causing setattr to re-allocate blocks endlessly until
            // the extent tree overflows (entries > max_entries → EIO).
            FileType::RegularFile => {
                fs.retry_metadata_contention(|| fs.fs.write_data_only(inode_num, offset, buf))
            }
            _ => Err(SystemError::EINVAL),
        }
    }

    fn write_direct(
        &self,
        offset: usize,
        len: usize,
        buf: &[u8],
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        let len = core::cmp::min(len, buf.len());
        self.write_sync(offset, &buf[0..len])
    }

    fn fs(&self) -> Arc<dyn vfs::FileSystem> {
        self.inner.lock().concret_fs()
    }

    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn find(&self, name: &str) -> Result<Arc<dyn IndexNode>, SystemError> {
        let _operation = self.begin_operation()?;
        let _namespace = self.namespace_lock.lock();
        let mut guard = self.inner.lock();
        let dname = DName::from(name);
        if let Some(child) = guard.children.get(&dname) {
            let child = child.clone();
            let fs = guard.concret_fs();
            if fs.validate_inode(&child).is_ok() {
                return Ok(child as Arc<dyn IndexNode>);
            }
            guard.children.remove(&dname);
        }
        let fs = guard.concret_fs();
        let next_inode = fs.fs.lookup(guard.inner_inode_num, name)?;
        // 通过self_ref获取Arc<Self>，然后转换为Arc<dyn IndexNode>
        let self_arc = guard.self_ref.upgrade().ok_or(SystemError::ENOENT)?;
        let inode =
            fs.get_or_create_inode(next_inode, dname.clone(), Some(Arc::downgrade(&self_arc)))?;
        guard.children.insert(dname, inode.clone());
        Ok(inode)
    }

    fn parent(&self) -> Result<Arc<dyn IndexNode>, SystemError> {
        // 只有目录才有父目录的概念
        // 先检查当前inode是否为目录
        let guard = self.inner.lock();

        // 如果存储了父级指针，直接返回
        if let Some(parent) = guard.parent.upgrade() {
            return Ok(parent);
        }

        Err(SystemError::ENOENT)
    }

    fn list(&self) -> Result<Vec<String>, SystemError> {
        let _operation = self.begin_operation()?;
        let guard = self.inner.lock();
        let dentry = guard.concret_fs().fs.listdir(guard.inner_inode_num)?;
        let mut list = Vec::new();
        for entry in dentry {
            list.push(entry.name());
        }
        Ok(list)
    }

    fn link(&self, name: &str, other: &Arc<dyn IndexNode>) -> Result<(), SystemError> {
        let _operation = self.begin_operation()?;
        let _namespace = self.namespace_lock.lock();
        let mut guard = self.inner.lock();
        let fs = guard.concret_fs();
        let ext4 = &fs.fs;
        let inode_num = guard.inner_inode_num;

        let other_arc = other
            .clone()
            .downcast_arc::<LockedExt4Inode>()
            .ok_or(SystemError::EINVAL)?;
        let other_fs = other_arc.inner.lock().concret_fs();
        if !Arc::ptr_eq(&fs, &other_fs) {
            return Err(SystemError::EXDEV);
        }
        let other_lifecycle = other_arc.lifecycle().clone();
        let _link_mutation = other_lifecycle.lock_link_mutation();
        let _other_operation = other_arc.begin_operation()?;
        let other_inode_num = other_arc.inner.lock().inner_inode_num;

        let my_attr = ext4.getattr(inode_num)?;
        let other_attr = ext4.getattr(other_inode_num)?;

        if my_attr.ftype != another_ext4::FileType::Directory {
            return Err(SystemError::ENOTDIR);
        }

        if other_attr.ftype == another_ext4::FileType::Directory {
            return Err(SystemError::EISDIR);
        }

        if ext4.lookup(inode_num, name).is_ok() {
            return Err(SystemError::EEXIST);
        }

        fs.retry_metadata_contention(|| ext4.link(other_inode_num, inode_num, name))?;
        if other_attr.links == 0 {
            // The orphan-del transaction made this inode live again. Discard
            // the one-shot capability published by its previous final unlink
            // before the fd retention that enabled AT_EMPTY_PATH can vanish.
            other_arc.cancel_deferred_reclaim_after_relink();
        }

        let dname = DName::from(name);
        guard.children.insert(dname, other_arc);

        Ok(())
    }

    fn unlink(&self, name: &str) -> Result<(), SystemError> {
        let _operation = self.begin_operation()?;
        let _namespace = self.namespace_lock.lock();
        let mut guard = self.inner.lock();
        let fs = guard.concret_fs();
        let ext4 = &fs.fs;
        let inode_num = guard.inner_inode_num;
        let attr = ext4.getattr(inode_num)?;
        if attr.ftype != another_ext4::FileType::Directory {
            return Err(SystemError::ENOTDIR);
        }
        let target_num = ext4.lookup(inode_num, name)?;
        if ext4.getattr(target_num)?.ftype == FileType::Directory {
            return Err(SystemError::EISDIR);
        }
        let self_arc = guard.self_ref.upgrade().ok_or(SystemError::ENOENT)?;
        let target = fs.get_or_create_inode(
            target_num,
            DName::from(name),
            Some(Arc::downgrade(&self_arc)),
        )?;
        let target_lifecycle = target.lifecycle().clone();
        let _link_mutation = target_lifecycle.lock_link_mutation();
        let _target_operation = target.begin_operation()?;
        match ext4.lookup(inode_num, name) {
            Ok(current) if current == target_num => {}
            Ok(_) => return Err(SystemError::EAGAIN_OR_EWOULDBLOCK),
            Err(error) => return Err(error.into()),
        }
        let reclaim = fs.retry_metadata_contention(|| ext4.unlink(inode_num, name))?;
        target.handoff_namespace_reclaim(reclaim)?;
        // 清理 children 缓存
        let _ = guard.children.remove(&DName::from(name));
        Ok(())
    }

    fn metadata(&self) -> Result<vfs::Metadata, SystemError> {
        let _operation = self.begin_operation()?;
        let (fs, inode_num, vfs_inode_id, cached_size) = {
            let guard = self.inner.lock();
            (
                guard.concret_fs(),
                guard.inner_inode_num,
                guard.vfs_inode_id,
                guard.cached_file_size,
            )
        };
        let attr = fs.fs.getattr(inode_num)?;
        // Disk attributes provide non-cached fields. Read the authoritative
        // in-memory values afterwards so a concurrent atime update cannot be
        // hidden by a stale pre-getattr snapshot.
        let cached_times = self.inner.lock().cached_times;
        let size = cached_size.unwrap_or(attr.size);

        // dev_id: filesystem device number (st_dev)
        let dev_id = fs.raw_dev.data() as usize;

        // raw_dev: device node's rdev (st_rdev), only for char/block devices
        let raw_dev = if matches!(attr.ftype, FileType::CharacterDev | FileType::BlockDev) {
            let (major, minor) = attr.rdev;
            DeviceNumber::new(
                crate::driver::base::device::device_number::Major::new(major),
                minor,
            )
        } else {
            DeviceNumber::default()
        };

        Ok(vfs::Metadata {
            inode_id: vfs_inode_id,
            size: size as i64,
            blk_size: another_ext4::BLOCK_SIZE,
            blocks: attr.blocks as usize,
            atime: PosixTimeSpec::new(cached_times.atime.into(), 0),
            btime: PosixTimeSpec::new(attr.atime.into(), 0),
            mtime: PosixTimeSpec::new(cached_times.mtime.into(), 0),
            ctime: PosixTimeSpec::new(cached_times.ctime.into(), 0),
            file_type: Self::file_type(attr.ftype),
            mode: InodeMode::from_bits_truncate(attr.perm.bits() as u32),
            flags: InodeFlags::empty(),
            nlinks: attr.links as usize,
            uid: attr.uid as usize,
            gid: attr.gid as usize,
            dev_id,
            raw_dev,
        })
    }

    fn close(&self, _: PrivateData) -> Result<(), SystemError> {
        Ok(())
    }

    fn sync(&self) -> Result<(), SystemError> {
        let _operation = self.begin_operation()?;
        let snapshot = if let Some(page_cache) = self.page_cache() {
            let (snapshot, range) =
                page_cache
                    .manager()
                    .start_writeback_range_with_freeze(0, usize::MAX, || {
                        self.freeze_metadata(false)
                    })?;
            range.wait_for_completion()?;
            snapshot
        } else {
            self.freeze_metadata(false)?
        };
        self.flush_frozen_metadata(snapshot)?;
        let fs = self.inner.lock().concret_fs();
        fs.finish_sync_durability_boundary()
    }

    fn datasync(&self) -> Result<(), SystemError> {
        let _operation = self.begin_operation()?;
        let snapshot = if let Some(page_cache) = self.page_cache() {
            let (snapshot, range) =
                page_cache
                    .manager()
                    .start_writeback_range_with_freeze(0, usize::MAX, || {
                        self.freeze_metadata(true)
                    })?;
            range.wait_for_completion()?;
            snapshot
        } else {
            self.freeze_metadata(true)?
        };
        self.flush_frozen_metadata(snapshot)?;
        let fs = self.inner.lock().concret_fs();
        fs.finish_sync_durability_boundary()
    }

    fn sync_file(&self, datasync: bool, _data: PrivateData) -> Result<(), SystemError> {
        if datasync {
            self.datasync()
        } else {
            self.sync()
        }
    }

    fn sync_file_range(
        &self,
        start: usize,
        end: usize,
        datasync: bool,
        _data: PrivateData,
    ) -> Result<(), SystemError> {
        let _operation = self.begin_operation()?;
        let snapshot = if let Some(page_cache) = self.page_cache() {
            let start_index = start >> MMArch::PAGE_SHIFT;
            let end_index = end >> MMArch::PAGE_SHIFT;
            let (snapshot, range) = page_cache.manager().start_writeback_range_with_freeze(
                start_index,
                end_index,
                || self.freeze_metadata(datasync),
            )?;
            range.wait_for_completion()?;
            snapshot
        } else {
            self.freeze_metadata(datasync)?
        };
        self.flush_frozen_metadata(snapshot)?;
        let fs = self.inner.lock().concret_fs();
        fs.finish_sync_durability_boundary()
    }

    fn write_inode(&self, _wbc: &vfs::WritebackControl) -> Result<(), SystemError> {
        self.flush_metadata(false)
    }

    fn page_cache(&self) -> Option<Arc<PageCache>> {
        self.inner.lock().page_cache.clone()
    }

    fn set_metadata(&self, metadata: &vfs::Metadata) -> Result<(), SystemError> {
        let _operation = self.begin_operation()?;
        let _delalloc_admission = self.close_production_delalloc_admission()?;
        self.drain_delalloc_before_eager()?;
        let _io_guard = self.io_lock.lock();
        let _metadata_commit = self.metadata_commit_lock.lock();
        let mode = metadata.mode.union(InodeMode::from(metadata.file_type));

        let to_ext4_time =
            |time: &PosixTimeSpec| -> u32 { time.tv_sec.max(0).min(u32::MAX as i64) as u32 };

        let (fs, inode_num, before_atime_version, before_mtime_version, before_ctime_version) = {
            let guard = self.inner.lock();
            (
                guard.concret_fs(),
                guard.inner_inode_num,
                guard.cached_atime_version,
                guard.cached_mtime_version,
                guard.cached_ctime_version,
            )
        };
        let next_atime_version = before_atime_version
            .checked_add(1)
            .ok_or(SystemError::EOVERFLOW)?;
        let next_mtime_version = before_mtime_version
            .checked_add(1)
            .ok_or(SystemError::EOVERFLOW)?;
        let next_ctime_version = before_ctime_version
            .checked_add(1)
            .ok_or(SystemError::EOVERFLOW)?;
        let ext4 = &fs.fs;
        fs.retry_metadata_contention(|| {
            ext4.setattr(
                inode_num,
                another_ext4::SetAttr {
                    mode: Some(another_ext4::InodeMode::from_bits_truncate(
                        mode.bits() as u16
                    )),
                    uid: Some(metadata.uid as u32),
                    gid: Some(metadata.gid as u32),
                    size: Some(metadata.size as u64),
                    atime: Some(to_ext4_time(&metadata.atime)),
                    mtime: Some(to_ext4_time(&metadata.mtime)),
                    ctime: Some(to_ext4_time(&metadata.ctime)),
                    crtime: Some(to_ext4_time(&metadata.btime)),
                },
            )
        })?;
        {
            let mut guard = self.inner.lock();
            guard.cached_file_size = Some(metadata.size as u64);
            if guard.cached_atime_version == before_atime_version {
                guard.cached_times.atime = to_ext4_time(&metadata.atime);
                guard.cached_atime_version = next_atime_version;
                guard.durable_atime_version = guard.cached_atime_version;
                guard.dirty_state.remove(InodeDirtyState::ATIME_DIRTY);
            }
            if guard.cached_mtime_version == before_mtime_version {
                guard.cached_times.mtime = to_ext4_time(&metadata.mtime);
                guard.cached_mtime_version = next_mtime_version;
                guard.durable_mtime_version = guard.cached_mtime_version;
                guard.dirty_state.remove(InodeDirtyState::MTIME_DIRTY);
            }
            if guard.cached_ctime_version == before_ctime_version {
                guard.cached_times.ctime = to_ext4_time(&metadata.ctime);
                guard.cached_ctime_version = next_ctime_version;
                guard.durable_ctime_version = guard.cached_ctime_version;
                guard.dirty_state.remove(InodeDirtyState::CTIME_DIRTY);
            }
            guard.dirty_state.remove(InodeDirtyState::SIZE_DIRTY);
        }
        self.release_clean_metadata_queue_owner(&fs);

        Ok(())
    }

    fn set_metadata_masked(
        &self,
        metadata: &vfs::Metadata,
        mask: SetMetadataMask,
    ) -> Result<(), SystemError> {
        if mask.is_empty() {
            return Ok(());
        }

        let _operation = self.begin_operation()?;
        let _delalloc_admission = self.close_production_delalloc_admission()?;
        self.drain_delalloc_before_eager()?;
        let _io_guard = self.io_lock.lock();
        let _metadata_commit = self.metadata_commit_lock.lock();
        let to_ext4_time =
            |time: &PosixTimeSpec| -> u32 { time.tv_sec.max(0).min(u32::MAX as i64) as u32 };
        let (fs, inode_num, before_atime_version, before_mtime_version, before_ctime_version) = {
            let guard = self.inner.lock();
            (
                guard.concret_fs(),
                guard.inner_inode_num,
                guard.cached_atime_version,
                guard.cached_mtime_version,
                guard.cached_ctime_version,
            )
        };
        let mode = metadata.mode.union(InodeMode::from(metadata.file_type));
        let atime = mask
            .contains(SetMetadataMask::ATIME)
            .then(|| to_ext4_time(&metadata.atime));
        let mtime = mask
            .contains(SetMetadataMask::MTIME)
            .then(|| to_ext4_time(&metadata.mtime));
        let ctime = mask
            .contains(SetMetadataMask::CTIME)
            .then(|| to_ext4_time(&metadata.ctime));
        let next_atime_version = atime
            .map(|_| {
                before_atime_version
                    .checked_add(1)
                    .ok_or(SystemError::EOVERFLOW)
            })
            .transpose()?;
        let next_mtime_version = mtime
            .map(|_| {
                before_mtime_version
                    .checked_add(1)
                    .ok_or(SystemError::EOVERFLOW)
            })
            .transpose()?;
        let next_ctime_version = ctime
            .map(|_| {
                before_ctime_version
                    .checked_add(1)
                    .ok_or(SystemError::EOVERFLOW)
            })
            .transpose()?;

        fs.retry_metadata_contention(|| {
            fs.fs.setattr(
                inode_num,
                another_ext4::SetAttr {
                    mode: mask
                        .contains(SetMetadataMask::MODE)
                        .then(|| another_ext4::InodeMode::from_bits_truncate(mode.bits() as u16)),
                    uid: mask
                        .contains(SetMetadataMask::UID)
                        .then_some(metadata.uid as u32),
                    gid: mask
                        .contains(SetMetadataMask::GID)
                        .then_some(metadata.gid as u32),
                    atime,
                    mtime,
                    ctime,
                    ..Default::default()
                },
            )
        })?;

        {
            let mut guard = self.inner.lock();
            // Buffered reads/writes can update cached times without io_lock.
            // Preserve and leave dirty any value that changed while setattr
            // was in flight; writeback will then persist the newer value.
            if let (Some(atime), Some(next_atime_version)) = (atime, next_atime_version) {
                if guard.cached_atime_version == before_atime_version {
                    guard.cached_times.atime = atime;
                    guard.cached_atime_version = next_atime_version;
                    guard.durable_atime_version = guard.cached_atime_version;
                    guard.dirty_state.remove(InodeDirtyState::ATIME_DIRTY);
                }
            }
            if let (Some(mtime), Some(next_mtime_version)) = (mtime, next_mtime_version) {
                if guard.cached_mtime_version == before_mtime_version {
                    guard.cached_times.mtime = mtime;
                    guard.cached_mtime_version = next_mtime_version;
                    guard.durable_mtime_version = guard.cached_mtime_version;
                    guard.dirty_state.remove(InodeDirtyState::MTIME_DIRTY);
                }
            }
            if let (Some(ctime), Some(next_ctime_version)) = (ctime, next_ctime_version) {
                if guard.cached_ctime_version == before_ctime_version {
                    guard.cached_times.ctime = ctime;
                    guard.cached_ctime_version = next_ctime_version;
                    guard.durable_ctime_version = guard.cached_ctime_version;
                    guard.dirty_state.remove(InodeDirtyState::CTIME_DIRTY);
                }
            }
        }
        self.release_clean_metadata_queue_owner(&fs);
        Ok(())
    }

    fn update_atime(&self, now: PosixTimeSpec, relatime: bool) -> Result<(), SystemError> {
        let atime = now.tv_sec.max(0).min(u32::MAX as i64) as u32;
        let now = PosixTimeSpec::new(atime.into(), 0);
        let self_arc = {
            let guard = self.inner.lock();
            let times = guard.cached_times;
            if !vfs::should_update_atime(
                PosixTimeSpec::new(times.atime.into(), 0),
                PosixTimeSpec::new(times.mtime.into(), 0),
                PosixTimeSpec::new(times.ctime.into(), 0),
                now,
                relatime,
            ) {
                return Ok(());
            }
            guard.self_ref.upgrade().ok_or(SystemError::ENOENT)?
        };
        Ext4FileSystem::mark_inode_atime_dirty(&self_arc, atime, relatime)
    }

    fn resize(&self, len: usize) -> Result<(), SystemError> {
        let _operation = self.begin_operation()?;
        let _delalloc_admission = self.close_production_delalloc_admission()?;
        self.drain_delalloc_before_eager()?;
        let (fs, inode_num, page_cache) = {
            let guard = self.inner.lock();
            (
                guard.concret_fs(),
                guard.inner_inode_num,
                guard.page_cache.clone(),
            )
        };
        let apply_resize = || -> Result<(), SystemError> {
            let _io_guard = self.io_lock.lock();
            let ext4 = &fs.fs;
            // 仅调整文件大小，其他属性保持不变
            fs.retry_metadata_contention(|| {
                ext4.setattr(
                    inode_num,
                    another_ext4::SetAttr {
                        mode: None,
                        uid: None,
                        gid: None,
                        size: Some(len as u64),
                        atime: None,
                        mtime: None,
                        ctime: None,
                        crtime: None,
                    },
                )
            })?;
            // 更新缓存的文件大小
            {
                let mut guard = self.inner.lock();
                guard.cached_file_size = Some(len as u64);
                guard.dirty_state.remove(InodeDirtyState::SIZE_DIRTY);
            }
            self.release_clean_metadata_queue_owner(&fs);
            Ok(())
        };

        if let Some(page_cache) = page_cache {
            let hole_start_page = len
                .checked_add(MMArch::PAGE_SIZE - 1)
                .ok_or(SystemError::EFBIG)?
                >> MMArch::PAGE_SHIFT;
            let mut truncate_pending = false;
            loop {
                // Match PageCache::truncate(), but acquire ext4's size lock
                // after invalidate_write so mmap faults and regular writes
                // use one global order: invalidate -> size -> inode I/O.
                page_cache.unmap_mapping_pages_even_cow(hole_start_page, None)?;
                let (shrinking, committed) = {
                    let _invalidate = page_cache.invalidate_write();
                    let _size_guard = self.size_lock.write();
                    // Classify against the authoritative size while holding the
                    // same lock that serializes the update.  A function-entry
                    // snapshot can become stale after a concurrent extension.
                    let cached_size = self.inner.lock().cached_file_size;
                    let current_size = match cached_size {
                        Some(size) => size,
                        None => fs.fs.getattr(inode_num)?.size,
                    };
                    // After truncate_locked() asks for another unmap pass, the
                    // inode size already equals len.  Preserve that pending
                    // cache truncation unless a concurrent resize moved the
                    // authoritative size below this request.
                    let shrinking = len < current_size as usize
                        || (truncate_pending && len == current_size as usize);
                    apply_resize()?;
                    let committed = !shrinking || page_cache.truncate_locked(len)?;
                    (shrinking, committed)
                };
                if committed {
                    if shrinking {
                        page_cache.unmap_mapping_pages_even_cow(hole_start_page, None)?;
                    }
                    return Ok(());
                }
                truncate_pending = shrinking;
            }
        }
        let _size_guard = self.size_lock.write();
        apply_resize()
    }

    fn fallocate_file(
        &self,
        mode: i32,
        offset: usize,
        len: usize,
        lock_owner: u64,
        data: MutexGuard<FilePrivateData>,
    ) -> Result<(), SystemError> {
        drop(data);
        vfs::vcore::resize_based_fallocate(self, mode, offset, len, lock_owner)
    }

    fn truncate(&self, len: usize) -> Result<(), SystemError> {
        // 复用 resize 的实现
        self.resize(len)
    }

    fn rmdir(&self, name: &str) -> Result<(), SystemError> {
        let _operation = self.begin_operation()?;
        let _namespace = self.namespace_lock.lock();
        let mut guard = self.inner.lock();
        let fs = guard.concret_fs();
        let concret_fs = &fs.fs;
        let inode_num = guard.inner_inode_num;
        if concret_fs.getattr(inode_num)?.ftype != FileType::Directory {
            return Err(SystemError::ENOTDIR);
        }
        let target_num = concret_fs.lookup(inode_num, name)?;
        if target_num == inode_num {
            return Err(if name == "." {
                SystemError::EINVAL
            } else {
                SystemError::ENOTEMPTY
            });
        }
        if concret_fs.getattr(target_num)?.ftype != FileType::Directory {
            return Err(SystemError::ENOTDIR);
        }
        if concret_fs.listdir(target_num)?.len() > 2 {
            return Err(SystemError::ENOTEMPTY);
        }
        let self_arc = guard.self_ref.upgrade().ok_or(SystemError::ENOENT)?;
        let target = fs.get_or_create_inode(
            target_num,
            DName::from(name),
            Some(Arc::downgrade(&self_arc)),
        )?;
        let target_lifecycle = target.lifecycle().clone();
        let _link_mutation = target_lifecycle.lock_link_mutation();
        match concret_fs.lookup(inode_num, name) {
            Ok(current) if current == target_num => {}
            Ok(_) => return Err(SystemError::EAGAIN_OR_EWOULDBLOCK),
            Err(error) => return Err(error.into()),
        }
        let target_attr = concret_fs.getattr(target_num)?;
        if target_attr.ftype != FileType::Directory {
            return Err(SystemError::ENOTDIR);
        }
        match concret_fs.listdir(target_num) {
            Ok(entries) if entries.len() <= 2 => {}
            Ok(_) => return Err(SystemError::ENOTEMPTY),
            Err(error) => return Err(error.into()),
        }
        let reclaim = fs.retry_metadata_contention(|| concret_fs.rmdir(inode_num, name))?;
        target.handoff_namespace_reclaim(reclaim)?;
        // 清理 children 缓存
        let _ = guard.children.remove(&DName::from(name));

        Ok(())
    }

    fn dname(&self) -> Result<DName, SystemError> {
        Ok(self.inner.lock().dname.clone())
    }

    fn getxattr(&self, name: &str, buf: &mut [u8]) -> Result<usize, SystemError> {
        let _operation = self.begin_operation()?;
        let guard = self.inner.lock();
        let fs = guard.concret_fs();
        let ext4 = &fs.fs;
        let inode_num = guard.inner_inode_num;

        if ext4.getattr(inode_num)?.ftype == FileType::SymLink {
            return Err(SystemError::EPERM);
        }

        // 调用another_ext4库的getxattr接口
        let value = ext4.getxattr(inode_num, name)?;

        // 如果缓冲区为空，只返回需要的长度
        if buf.is_empty() {
            return Ok(value.len());
        }

        // 检查缓冲区大小是否足够
        if buf.len() < value.len() {
            return Err(SystemError::ERANGE);
        }

        // 复制数据到缓冲区
        let copy_len = core::cmp::min(buf.len(), value.len());
        buf[..copy_len].copy_from_slice(&value[..copy_len]);

        Ok(copy_len)
    }

    fn setxattr(&self, name: &str, value: &[u8], flags: XattrFlags) -> Result<usize, SystemError> {
        let _operation = self.begin_operation()?;
        let guard = self.inner.lock();
        let fs = guard.concret_fs();
        let ext4 = &fs.fs;
        let inode_num = guard.inner_inode_num;

        if ext4.getattr(inode_num)?.ftype == FileType::SymLink {
            return Err(SystemError::EPERM);
        }

        fs.retry_metadata_contention(|| {
            ext4.setxattr_with_flags(
                inode_num,
                name,
                value,
                flags.contains(XattrFlags::CREATE),
                flags.contains(XattrFlags::REPLACE),
            )
        })?;

        Ok(0)
    }

    fn listxattr(&self, buf: &mut [u8]) -> Result<usize, SystemError> {
        let _operation = self.begin_operation()?;
        let guard = self.inner.lock();
        let ext4 = &guard.concret_fs().fs;
        let inode_num = guard.inner_inode_num;

        let names = ext4.listxattr(inode_num)?;
        let total_len = names.iter().try_fold(0usize, |acc, name| {
            acc.checked_add(name.len())
                .and_then(|len| len.checked_add(1))
                .ok_or(SystemError::E2BIG)
        })?;

        if buf.is_empty() {
            return Ok(total_len);
        }
        if buf.len() < total_len {
            return Err(SystemError::ERANGE);
        }

        let mut offset = 0;
        for name in names {
            let name_bytes = name.as_bytes();
            let next = offset + name_bytes.len();
            buf[offset..next].copy_from_slice(name_bytes);
            buf[next] = 0;
            offset = next + 1;
        }

        Ok(total_len)
    }

    fn removexattr(&self, name: &str) -> Result<usize, SystemError> {
        let _operation = self.begin_operation()?;
        let guard = self.inner.lock();
        let fs = guard.concret_fs();
        let ext4 = &fs.fs;
        let inode_num = guard.inner_inode_num;

        if ext4.getattr(inode_num)?.ftype == FileType::SymLink {
            return Err(SystemError::EPERM);
        }

        fs.retry_metadata_contention(|| ext4.removexattr(inode_num, name))?;
        Ok(0)
    }

    fn mknod(
        &self,
        filename: &str,
        mode: InodeMode,
        dev_t: DeviceNumber,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        let file_type = vfs::FileType::from(mode);
        if file_type == vfs::FileType::File {
            return self.create(filename, vfs::FileType::File, mode);
        }
        let _operation = self.begin_operation()?;
        let _io = self.io_lock.lock();
        let _namespace = self.namespace_lock.lock();
        let parent_metadata = self.metadata()?;
        let init = vfs::permission::child_inode_init(&parent_metadata, file_type, mode);

        let mut guard = self.inner.lock();
        let fs = guard.concret_fs();
        let _reuse = fs.begin_allocation()?;
        let ext4 = &fs.fs;
        let inode_num = guard.inner_inode_num;
        // Resolve the parent lifetime before publishing the on-disk name so
        // no fallible parent lookup remains after the namespace transaction.
        let self_arc = guard.self_ref.upgrade().ok_or(SystemError::ENOENT)?;

        if ext4.getattr(inode_num)?.ftype != FileType::Directory {
            return Err(SystemError::ENOTDIR);
        }

        // VFS InodeMode(u32) → another_ext4 InodeMode(u16)
        let file_mode = another_ext4::InodeMode::from_bits_truncate(init.mode.bits() as u16);

        // Create inode based on file type
        let attr = if matches!(
            file_type,
            vfs::FileType::CharDevice | vfs::FileType::BlockDevice
        ) {
            // Character/block device: use mknod to store device number in i_block
            fs.retry_metadata_contention(|| {
                ext4.mknod_with_owner_and_attr(
                    inode_num,
                    filename,
                    file_mode,
                    dev_t.major().data(),
                    dev_t.minor(),
                    another_ext4::InodeOwner {
                        uid: init.uid as u32,
                        gid: init.gid as u32,
                    },
                )
            })?
        } else {
            // FIFO, Socket, etc.: use regular create (no device number needed)
            fs.retry_metadata_contention(|| {
                ext4.create_with_owner_and_attr(
                    inode_num,
                    filename,
                    file_mode,
                    another_ext4::InodeOwner {
                        uid: init.uid as u32,
                        gid: init.gid as u32,
                    },
                )
            })?
        };

        // Wrap as VFS inode and cache
        let dname = DName::from(filename);
        let inode = fs.publish_allocated_inode(
            attr,
            dname.clone(),
            Some(Arc::downgrade(&self_arc)),
            &_reuse,
        )?;
        guard.children.insert(dname, inode.clone());
        drop(guard);
        Ok(inode as Arc<dyn IndexNode>)
    }

    fn special_node(&self) -> Option<SpecialNodeData> {
        self.inner.lock().special_node.clone()
    }

    fn move_to(
        &self,
        old_name: &str,
        target: &Arc<dyn IndexNode>,
        new_name: &str,
        flags: RenameFlags,
    ) -> Result<(), SystemError> {
        let _operation = self.begin_operation()?;
        let _source_io = self.io_lock.lock();
        let whiteout_init = if flags.contains(RenameFlags::WHITEOUT) {
            Some(vfs::permission::child_inode_init(
                &self.metadata()?,
                vfs::FileType::CharDevice,
                InodeMode::S_IFCHR | InodeMode::from_bits_truncate(0o600),
            ))
        } else {
            None
        };
        let target_locked = target
            .clone()
            .downcast_arc::<LockedExt4Inode>()
            .ok_or(SystemError::EXDEV)?;
        let _target_operation = target_locked.begin_operation()?;

        let (ext4_fs, src_inode_num) = {
            let guard = self.inner.lock();
            (guard.concret_fs(), guard.inner_inode_num)
        };
        let ext4 = &ext4_fs.fs;
        let target_inode_num = target_locked.inner.lock().inner_inode_num;
        if !Arc::ptr_eq(&ext4_fs, &target_locked.inner.lock().concret_fs()) {
            return Err(SystemError::EXDEV);
        }

        let (_first_namespace, _second_namespace) = if src_inode_num == target_inode_num {
            (self.namespace_lock.lock(), None)
        } else if src_inode_num < target_inode_num {
            (
                self.namespace_lock.lock(),
                Some(target_locked.namespace_lock.lock()),
            )
        } else {
            (
                target_locked.namespace_lock.lock(),
                Some(self.namespace_lock.lock()),
            )
        };

        let old_dname = DName::from(old_name);
        let new_dname = DName::from(new_name);

        // NOREPLACE check (VFS layer responsibility - ext4 lib doesn't know about flags)
        if flags.contains(RenameFlags::NOREPLACE) && ext4.lookup(target_inode_num, new_name).is_ok()
        {
            return Err(SystemError::EEXIST);
        }

        // Same directory, same name -> no-op
        if src_inode_num == target_inode_num && old_dname == new_dname {
            return Ok(());
        }

        // RENAME_EXCHANGE: 原子交换两个文件/目录
        if flags.contains(RenameFlags::EXCHANGE) {
            // VFS 层已验证目标存在，直接调用 exchange
            ext4_fs.retry_metadata_contention(|| {
                ext4.rename_exchange(src_inode_num, old_name, target_inode_num, new_name)
            })?;

            // 更新缓存：交换两个条目
            self.update_exchange_cache(
                &target_locked,
                src_inode_num,
                target_inode_num,
                &old_dname,
                &new_dname,
            );
            return Ok(());
        }

        // Capture the replacement target while both parent namespace locks are held.
        let dst_inode_num = ext4.lookup(target_inode_num, new_name).ok();
        let src_child_num = ext4.lookup(src_inode_num, old_name)?;
        if dst_inode_num == Some(src_child_num) {
            return Ok(());
        }
        let had_dst = dst_inode_num.is_some();
        let dst_inode = if let Some(dst_inode_num) = dst_inode_num {
            let target_parent = target_locked
                .inner
                .lock()
                .self_ref
                .upgrade()
                .ok_or(SystemError::ENOENT)?;
            Some(ext4_fs.get_or_create_inode(
                dst_inode_num,
                new_dname.clone(),
                Some(Arc::downgrade(&target_parent)),
            )?)
        } else {
            None
        };
        let dst_lifecycle = dst_inode.as_ref().map(|inode| inode.lifecycle().clone());
        let _dst_link_mutation = dst_lifecycle
            .as_ref()
            .map(|lifecycle| lifecycle.lock_link_mutation());
        if let Some(dst_inode_num) = dst_inode_num {
            let src_type = ext4.getattr(src_child_num)?.ftype;
            let dst_type = ext4.getattr(dst_inode_num)?.ftype;
            match (
                src_type == FileType::Directory,
                dst_type == FileType::Directory,
            ) {
                (true, false) => return Err(SystemError::ENOTDIR),
                (false, true) => return Err(SystemError::EISDIR),
                (true, true) if ext4.listdir(dst_inode_num)?.len() > 2 => {
                    return Err(SystemError::ENOTEMPTY);
                }
                _ => {}
            }
        }

        let mut resulting_whiteout = None;
        if flags.contains(RenameFlags::WHITEOUT) {
            let whiteout_init = whiteout_init.as_ref().ok_or(SystemError::EIO)?;
            let mut temp_name = String::new();
            let mut whiteout_inode = None;
            let source_parent = self
                .inner
                .lock()
                .self_ref
                .upgrade()
                .ok_or(SystemError::ENOENT)?;
            for _ in 0..32 {
                let candidate = format!(".dragonos-whiteout-{}", generate_inode_id().data());
                if ext4.lookup(src_inode_num, &candidate).is_ok() {
                    continue;
                }
                let allocation = ext4_fs.begin_allocation()?;
                let whiteout_attr = ext4_fs.retry_metadata_contention(|| {
                    ext4.mknod_with_owner_and_attr(
                        src_inode_num,
                        &candidate,
                        another_ext4::InodeMode::CHARDEV
                            | another_ext4::InodeMode::from_bits_retain(0o600),
                        WHITEOUT_DEV.major().data(),
                        WHITEOUT_DEV.minor(),
                        another_ext4::InodeOwner {
                            uid: whiteout_init.uid as u32,
                            gid: whiteout_init.gid as u32,
                        },
                    )
                })?;
                whiteout_inode = match ext4_fs.publish_allocated_inode(
                    whiteout_attr,
                    DName::from(candidate.as_str()),
                    Some(Arc::downgrade(&source_parent)),
                    &allocation,
                ) {
                    Ok(inode) => Some(inode),
                    Err(error) => {
                        drop(allocation);
                        let _reclaim = ext4_fs.begin_reclaim();
                        let cleanup = ext4_fs
                            .retry_metadata_contention(|| ext4.unlink(src_inode_num, &candidate))
                            .and_then(|handle| match handle {
                                Some(handle) => {
                                    Self::reclaim_with_metadata_contention_retry(&ext4_fs, handle)
                                        .map_err(|failure| SystemError::from(failure.0))
                                }
                                None => Ok(()),
                            });
                        if cleanup.is_err() {
                            ext4_fs.fail_stop_lifecycle();
                            return Err(SystemError::EIO);
                        }
                        return Err(error);
                    }
                };
                temp_name = candidate;
                break;
            }
            if temp_name.is_empty() {
                return Err(SystemError::EEXIST);
            }

            if let Err(err) = ext4_fs.retry_metadata_contention(|| {
                ext4.rename_exchange(src_inode_num, old_name, src_inode_num, &temp_name)
            }) {
                Self::reclaim_temporary_inode(
                    &ext4_fs,
                    src_inode_num,
                    &temp_name,
                    whiteout_inode.take().unwrap(),
                )?;
                return Err(err);
            }
            let rename_handle = match ext4_fs.retry_metadata_contention(|| {
                ext4.rename(src_inode_num, &temp_name, target_inode_num, new_name)
            }) {
                Ok(handle) => handle,
                Err(rename_error) => {
                    let rollback = ext4_fs.retry_metadata_contention(|| {
                        ext4.rename_exchange(src_inode_num, old_name, src_inode_num, &temp_name)
                    });
                    if rollback.is_err() {
                        let whiteout_tombstone = ext4_fs.begin_freeing(
                            whiteout_inode.as_ref().expect("whiteout was published"),
                        )?;
                        let _ = ext4_fs.poison_freeing(whiteout_tombstone, SystemError::EIO);
                        return Err(SystemError::EIO);
                    }
                    Self::reclaim_temporary_inode(
                        &ext4_fs,
                        src_inode_num,
                        &temp_name,
                        whiteout_inode.take().unwrap(),
                    )?;
                    return Err(rename_error);
                }
            };
            if let Some(dst_inode) = &dst_inode {
                dst_inode.handoff_namespace_reclaim(rename_handle)?;
            } else if let Some(handle) = rename_handle {
                // The destination was absent while both namespace locks were
                // held, so the backend must not report a replaced lifetime.
                // If it does, retain that orphan capability and fail-stop.
                return Self::quarantine_unexpected_rename_reclaim(&ext4_fs, handle);
            }
            if let Some(whiteout) = &whiteout_inode {
                whiteout.inner.lock().dname = old_dname.clone();
            }
            resulting_whiteout = whiteout_inode;
        } else {
            if let Some(dst_inode) = &dst_inode {
                let reclaim = ext4_fs.retry_metadata_contention(|| {
                    ext4.rename(src_inode_num, old_name, target_inode_num, new_name)
                })?;
                dst_inode.handoff_namespace_reclaim(reclaim)?;
            } else {
                // ext4 library now correctly handles atomic replace
                let reclaim = ext4_fs.retry_metadata_contention(|| {
                    ext4.rename(src_inode_num, old_name, target_inode_num, new_name)
                })?;
                if let Some(handle) = reclaim {
                    return Self::quarantine_unexpected_rename_reclaim(&ext4_fs, handle);
                }
            }
        }

        // Update cache
        self.update_rename_cache(
            &target_locked,
            src_inode_num,
            target_inode_num,
            &old_dname,
            &new_dname,
            had_dst,
        );
        if let Some(whiteout) = resulting_whiteout {
            self.inner.lock().children.insert(old_dname, whiteout);
        }
        Ok(())
    }
}

impl LockedExt4Inode {
    pub(super) fn close_production_delalloc_admission(
        &self,
    ) -> Result<ProductionDelallocAdmissionGuard<'_>, SystemError> {
        let _io = self.io_lock.lock();
        let mut guard = self.inner.lock();
        guard.delalloc.production.admission_closed = guard
            .delalloc
            .production
            .admission_closed
            .checked_add(1)
            .ok_or(SystemError::EOVERFLOW)?;
        Ok(ProductionDelallocAdmissionGuard { inode: self })
    }

    fn try_merge_delalloc_tail_segment(
        &self,
        page_cache: &Arc<PageCache>,
        offset: usize,
        buf: &[u8],
    ) -> Result<Option<usize>, SystemError> {
        if buf.is_empty()
            || (offset & (MMArch::PAGE_SIZE - 1))
                .checked_add(buf.len())
                .is_none_or(|end| end > MMArch::PAGE_SIZE)
        {
            return Ok(None);
        }
        let page_start = offset & !(MMArch::PAGE_SIZE - 1);
        let new_eof = offset.checked_add(buf.len()).ok_or(SystemError::EFBIG)? as u64;
        let write_time = PosixTimeSpec::now().tv_sec.to_u32().unwrap_or(0);
        let _invalidate = page_cache.invalidate_write();
        let _size = self.size_lock.read();
        let _io = self.io_lock.lock();
        let (fs, sequence, certificate) = {
            let guard = self.inner.lock();
            if guard.delalloc.production.admission_closed != 0
                || guard.cached_file_size != Some(offset as u64)
            {
                return Ok(None);
            }
            let Some((&tail_offset, tail)) = guard.delalloc.production.entries.last_key_value()
            else {
                return Ok(None);
            };
            if tail_offset != page_start {
                return Ok(None);
            }
            let ProductionDelallocEntryState::Ready(pending) = &tail.state else {
                return Ok(None);
            };
            let Some(certificate) = pending.certificate else {
                return Err(SystemError::EIO);
            };
            // Refuse exhaustion before PageCache publishes the merged bytes.
            guard
                .cached_mtime_version
                .checked_add(1)
                .ok_or(SystemError::EOVERFLOW)?;
            guard
                .cached_ctime_version
                .checked_add(1)
                .ok_or(SystemError::EOVERFLOW)?;
            (guard.concret_fs(), tail.sequence, certificate)
        };
        drop(_io);
        drop(_size);

        let mut merged = false;
        let publication = page_cache.write_single_page_segment_with_transition(
            offset,
            buf,
            PageCacheExpectedDirtyTransition::Merge(certificate),
            |transition| {
                merged = transition.kind()
                    == crate::filesystem::page_cache::PageCacheDirtyTransitionKind::MergedIntoDirty
                    && transition.dirty_incarnation() == certificate.dirty_incarnation()
            },
        );
        let _size = self.size_lock.read();
        let _io = self.io_lock.lock();
        match publication {
            Ok(written) if merged => {
                let mut guard = self.inner.lock();
                // Another metadata publisher may have advanced the version
                // while PageCache held the page lock.  Recompute before
                // moving the linear reservation out of the queue.  At this
                // point merged bytes are already dirty, so an unexpected
                // exhaustion is terminal rather than a recoverable partial
                // queue mutation.
                let Some(mtime_version) = guard.cached_mtime_version.checked_add(1) else {
                    drop(guard);
                    fs.fail_stop_lifecycle();
                    self.delalloc_progress
                        .publish(PageCacheWritebackProgressOutcome::Failed(SystemError::EIO));
                    return Err(SystemError::EIO);
                };
                let Some(ctime_version) = guard.cached_ctime_version.checked_add(1) else {
                    drop(guard);
                    fs.fail_stop_lifecycle();
                    self.delalloc_progress
                        .publish(PageCacheWritebackProgressOutcome::Failed(SystemError::EIO));
                    return Err(SystemError::EIO);
                };
                let entry = guard
                    .delalloc
                    .production
                    .entries
                    .remove(&page_start)
                    .ok_or(SystemError::EIO)?;
                let mut pending = match entry {
                    ProductionDelallocEntry {
                        sequence: current,
                        state: ProductionDelallocEntryState::Ready(pending),
                    } if current == sequence && pending.certificate == Some(certificate) => pending,
                    entry => {
                        guard.delalloc.production.entries.insert(page_start, entry);
                        drop(guard);
                        fs.fail_stop_lifecycle();
                        self.delalloc_progress
                            .publish(PageCacheWritebackProgressOutcome::Failed(SystemError::EIO));
                        return Err(SystemError::EIO);
                    }
                };
                pending.durable_eof = new_eof;
                pending.mtime = write_time;
                pending.ctime = write_time;
                guard.cached_file_size = Some(new_eof);
                guard.cached_times.mtime = write_time;
                guard.cached_times.ctime = write_time;
                guard.cached_mtime_version = mtime_version;
                guard.cached_ctime_version = ctime_version;
                pending.mtime_version = guard.cached_mtime_version;
                pending.ctime_version = guard.cached_ctime_version;
                guard.delalloc.production.entries.insert(
                    page_start,
                    ProductionDelallocEntry {
                        sequence,
                        state: ProductionDelallocEntryState::Ready(pending),
                    },
                );
                Ok(Some(written))
            }
            Ok(_) => {
                fs.fail_stop_lifecycle();
                self.delalloc_progress
                    .publish(PageCacheWritebackProgressOutcome::Failed(SystemError::EIO));
                Err(SystemError::EIO)
            }
            Err(SystemError::EAGAIN_OR_EWOULDBLOCK) => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn try_write_delalloc_new_page_segment(
        &self,
        page_cache: &Arc<PageCache>,
        offset: usize,
        buf: &[u8],
    ) -> Result<Option<usize>, SystemError> {
        let page_offset = offset & (MMArch::PAGE_SIZE - 1);
        if buf.is_empty()
            || page_offset
                .checked_add(buf.len())
                .is_none_or(|end| end > MMArch::PAGE_SIZE)
            || MMArch::PAGE_SIZE != another_ext4::BLOCK_SIZE
        {
            return Ok(None);
        }

        let mut operation = Some(self.begin_operation()?);
        let page_start = offset & !(MMArch::PAGE_SIZE - 1);
        let new_eof = offset.checked_add(buf.len()).ok_or(SystemError::EFBIG)? as u64;
        let write_time = PosixTimeSpec::now().tv_sec.to_u32().unwrap_or(0);
        'admission: loop {
            let _invalidate = page_cache.invalidate_write();
            let _size = self.size_lock.read();
            let _io = self.io_lock.lock();
            let (
                fs,
                inode_num,
                self_arc,
                old_size,
                old_mtime,
                old_ctime,
                mtime_version,
                ctime_version,
                expected_durable_eof_before,
                was_empty,
                sequence,
            ) = {
                let guard = self.inner.lock();
                if guard.delalloc.production.admission_closed != 0
                    || guard.dirty_state.intersects(
                        InodeDirtyState::SIZE_DIRTY
                            | InodeDirtyState::MTIME_DIRTY
                            | InodeDirtyState::CTIME_DIRTY,
                    )
                {
                    return Ok(None);
                }
                let fs = guard.concret_fs();
                if !fs.delalloc_admission_open() {
                    return Ok(None);
                }
                let old_size = guard.cached_file_size.ok_or(SystemError::EIO)?;
                if old_size as usize > offset {
                    return Ok(None);
                }
                if guard.delalloc.production.head_is_claimed() {
                    let page_cache = guard
                        .page_cache
                        .as_ref()
                        .map(Arc::downgrade)
                        .unwrap_or_default();
                    let progress = self
                        .delalloc_progress
                        .ticket(guard.self_ref.clone(), page_cache);
                    drop(guard);
                    drop(_io);
                    drop(_size);
                    drop(_invalidate);
                    let wait_outcome = progress.wait_for_progress_interruptible();
                    match wait_outcome? {
                        PageCacheWritebackProgressOutcome::Failed(error) => return Err(error),
                        PageCacheWritebackProgressOutcome::Progress
                        | PageCacheWritebackProgressOutcome::Cancelled => {
                            let current = ProcessManager::current_pcb();
                            if current.has_pending_signal_fast()
                                && current.has_pending_not_masked_signal()
                            {
                                return Err(SystemError::ERESTARTSYS);
                            }
                            continue 'admission;
                        }
                    }
                }
                let expected_durable_eof_before = guard
                    .delalloc
                    .production
                    .entries
                    .last_key_value()
                    .map(|(_, entry)| match &entry.state {
                        ProductionDelallocEntryState::Prepared(pending)
                        | ProductionDelallocEntryState::Ready(pending) => pending.durable_eof,
                        ProductionDelallocEntryState::Claimed { durable_eof, .. } => *durable_eof,
                    })
                    .unwrap_or(old_size);
                let sequence = guard.delalloc.production.next_sequence;
                sequence.checked_add(1).ok_or(SystemError::EOVERFLOW)?;
                let mtime_version = guard
                    .cached_mtime_version
                    .checked_add(1)
                    .ok_or(SystemError::EOVERFLOW)?;
                let ctime_version = guard
                    .cached_ctime_version
                    .checked_add(1)
                    .ok_or(SystemError::EOVERFLOW)?;
                (
                    fs,
                    guard.inner_inode_num,
                    guard.self_ref.upgrade().ok_or(SystemError::ESTALE)?,
                    old_size,
                    guard.cached_times.mtime,
                    guard.cached_times.ctime,
                    mtime_version,
                    ctime_version,
                    expected_durable_eof_before,
                    guard.delalloc.production.entries.is_empty(),
                    sequence,
                )
            };
            let authority = fs
                .delalloc_mapper_authority
                .as_ref()
                .ok_or(SystemError::EOPNOTSUPP_OR_ENOTSUP)?;
            if was_empty && self.delalloc_pool.lock().is_none() {
                // Pool creation snapshots the inode's current right spine
                // under another_ext4's non-blocking direct-metadata gate.
                // A concurrent journal owner (for example syncfs draining the
                // previous head) makes that snapshot temporarily unavailable;
                // this is internal metadata contention, not userspace
                // nonblocking I/O, so keep the write admission pending until
                // the owner releases the gate.
                let pool = fs.retry_metadata_contention(|| {
                    fs.fs
                        .create_delalloc_extent_node_pool_authorized(authority, inode_num)
                })?;
                let mut slot = self.delalloc_pool.lock();
                if slot.is_none() {
                    *slot = Some(pool);
                }
            }
            let observed = fs.fs.metadata_mutation_generation();
            let reservation_result = {
                let mut slot = self.delalloc_pool.lock();
                let pool = slot.as_mut().ok_or(SystemError::EIO)?;
                fs.fs.reserve_delalloc_append_block_projected_authorized(
                    authority,
                    inode_num,
                    page_start,
                    expected_durable_eof_before,
                    pool,
                )
            };
            let reservation = match reservation_result {
                Ok(reservation) => reservation,
                Err(error) if error.code() == another_ext4::ErrCode::EAGAIN => {
                    drop(_io);
                    drop(_size);
                    drop(_invalidate);
                    fs.wait_metadata_mutation_progress(observed)?;
                    continue 'admission;
                }
                Err(error)
                    if matches!(
                        error.code(),
                        another_ext4::ErrCode::EINVAL | another_ext4::ErrCode::ENOTSUP
                    ) =>
                {
                    if was_empty {
                        self.release_empty_delalloc_pool(&fs, authority)?;
                    }
                    return Ok(None);
                }
                Err(error) => {
                    if was_empty {
                        self.release_empty_delalloc_pool(&fs, authority)?;
                    }
                    return Err(error.into());
                }
            };
            let sequence = {
                let mut guard = self.inner.lock();
                let tail_eof = guard
                    .delalloc
                    .production
                    .entries
                    .last_key_value()
                    .map(|(_, entry)| match &entry.state {
                        ProductionDelallocEntryState::Prepared(pending)
                        | ProductionDelallocEntryState::Ready(pending) => pending.durable_eof,
                        ProductionDelallocEntryState::Claimed { durable_eof, .. } => *durable_eof,
                    })
                    .unwrap_or(old_size);
                if guard.delalloc.production.admission_closed != 0
                    || guard.cached_file_size != Some(old_size)
                    || guard.cached_times.mtime != old_mtime
                    || guard.cached_times.ctime != old_ctime
                    || tail_eof != expected_durable_eof_before
                    || guard.delalloc.production.entries.contains_key(&page_start)
                    || guard.delalloc.production.entries.is_empty() != was_empty
                {
                    drop(guard);
                    let mut reservation = reservation;
                    self.cancel_projected_delalloc_reservation(&fs, authority, &mut reservation)?;
                    return Ok(None);
                }
                if guard.delalloc.production.next_sequence != sequence {
                    drop(guard);
                    let mut reservation = reservation;
                    self.cancel_projected_delalloc_reservation(&fs, authority, &mut reservation)?;
                    return Ok(None);
                }
                guard.delalloc.production.next_sequence = sequence + 1;
                if was_empty {
                    let Some(queue_operation) = operation.take() else {
                        drop(guard);
                        let mut reservation = reservation;
                        self.cancel_projected_delalloc_reservation(
                            &fs,
                            authority,
                            &mut reservation,
                        )?;
                        return Err(SystemError::EIO);
                    };
                    guard.delalloc.production.queue_operation = Some(queue_operation);
                }
                guard.delalloc.production.entries.insert(
                    page_start,
                    ProductionDelallocEntry {
                        sequence,
                        state: ProductionDelallocEntryState::Prepared(ProductionDelallocPending {
                            reservation,
                            certificate: None,
                            offset: page_start,
                            durable_eof: new_eof,
                            mtime: write_time,
                            ctime: write_time,
                            mtime_version,
                            ctime_version,
                        }),
                    },
                );
                sequence
            };
            if was_empty {
                if let Err(error) = fs.register_delalloc_inode(inode_num, self_arc) {
                    let (mut pending, owner) = {
                        let mut guard = self.inner.lock();
                        let entry = guard
                            .delalloc
                            .production
                            .entries
                            .remove(&page_start)
                            .ok_or(SystemError::EIO)?;
                        let owner = guard.delalloc.production.queue_operation.take();
                        let pending = match entry {
                            ProductionDelallocEntry {
                                sequence: current,
                                state: ProductionDelallocEntryState::Prepared(pending),
                            } if current == sequence => pending,
                            _ => return Err(SystemError::EIO),
                        };
                        (pending, owner)
                    };
                    drop(_io);
                    drop(_size);
                    self.cancel_projected_delalloc_reservation(
                        &fs,
                        authority,
                        &mut pending.reservation,
                    )?;
                    self.release_empty_delalloc_pool(&fs, authority)?;
                    drop(pending);
                    drop(owner);
                    return Err(error);
                }
            }

            drop(_io);
            drop(_size);
            let mut published = None;
            let publication = page_cache.write_single_page_segment_with_transition(
                offset,
                buf,
                PageCacheExpectedDirtyTransition::Start,
                |transition| published = transition.certificate(),
            );
            let _size = self.size_lock.read();
            let _io = self.io_lock.lock();
            return match publication {
                Ok(written) => {
                    let Some(certificate) = published else {
                        fs.fail_stop_lifecycle();
                        return Err(SystemError::EIO);
                    };
                    let mut guard = self.inner.lock();
                    let entry = guard
                        .delalloc
                        .production
                        .entries
                        .remove(&page_start)
                        .ok_or(SystemError::EIO)?;
                    let mut pending = match entry {
                        ProductionDelallocEntry {
                            sequence: current,
                            state: ProductionDelallocEntryState::Prepared(pending),
                        } if current == sequence => pending,
                        _ => {
                            fs.fail_stop_lifecycle();
                            return Err(SystemError::EIO);
                        }
                    };
                    pending.certificate = Some(certificate);
                    guard.cached_file_size = Some(new_eof);
                    guard.cached_times.mtime = write_time;
                    guard.cached_times.ctime = write_time;
                    guard.cached_mtime_version = pending.mtime_version;
                    guard.cached_ctime_version = pending.ctime_version;
                    guard.delalloc.production.entries.insert(
                        page_start,
                        ProductionDelallocEntry {
                            sequence,
                            state: ProductionDelallocEntryState::Ready(pending),
                        },
                    );
                    Ok(Some(written))
                }
                Err(error) => {
                    let (mut pending, became_empty, owner) = {
                        let mut guard = self.inner.lock();
                        let entry = guard
                            .delalloc
                            .production
                            .entries
                            .remove(&page_start)
                            .ok_or(SystemError::EIO)?;
                        let pending = match entry {
                            ProductionDelallocEntry {
                                sequence: current,
                                state: ProductionDelallocEntryState::Prepared(pending),
                            } if current == sequence => pending,
                            _ => {
                                fs.fail_stop_lifecycle();
                                return Err(SystemError::EIO);
                            }
                        };
                        let became_empty = guard.delalloc.production.entries.is_empty();
                        let owner = became_empty
                            .then(|| guard.delalloc.production.queue_operation.take())
                            .flatten();
                        (pending, became_empty, owner)
                    };
                    drop(_io);
                    drop(_size);
                    self.cancel_projected_delalloc_reservation(
                        &fs,
                        authority,
                        &mut pending.reservation,
                    )?;
                    if became_empty {
                        self.release_empty_delalloc_pool(&fs, authority)?;
                        fs.unregister_delalloc_inode(inode_num);
                    }
                    drop(pending);
                    drop(owner);
                    if error == SystemError::EAGAIN_OR_EWOULDBLOCK {
                        Ok(None)
                    } else {
                        Err(error)
                    }
                }
            };
        }
    }

    fn cancel_projected_delalloc_reservation(
        &self,
        fs: &Arc<Ext4FileSystem>,
        authority: &another_ext4::DelallocAppendMapperAuthority,
        reservation: &mut another_ext4::DelallocAppendBlockReservation,
    ) -> Result<(), SystemError> {
        {
            let mut slot = self.delalloc_pool.lock();
            let pool = slot.as_mut().ok_or(SystemError::EIO)?;
            if fs
                .fs
                .cancel_projected_delalloc_append_block_authorized(authority, reservation, pool)
                .is_ok()
            {
                return Ok(());
            }
        }
        fs.fail_stop_lifecycle();
        fs.fs
            .terminalize_delalloc_append_block_authorized_after_fail_stop(authority, reservation)
            .map_err(SystemError::from)
    }

    fn release_empty_delalloc_pool(
        &self,
        fs: &Arc<Ext4FileSystem>,
        authority: &another_ext4::DelallocAppendMapperAuthority,
    ) -> Result<(), SystemError> {
        let Some(mut pool) = self.delalloc_pool.lock().take() else {
            return Ok(());
        };
        if fs
            .fs
            .release_delalloc_extent_node_pool_authorized(authority, &mut pool)
            .is_ok()
        {
            return Ok(());
        }
        fs.fail_stop_lifecycle();
        fs.fs
            .terminalize_delalloc_extent_node_pool_authorized_after_fail_stop(authority, &mut pool)
            .map_err(SystemError::from)
    }

    pub(super) fn drain_delalloc_before_eager(&self) -> Result<(), SystemError> {
        loop {
            let (page_cache, first_page, last_page) = {
                let guard = self.inner.lock();
                let Some((_, head)) = guard.delalloc.production.head() else {
                    return Ok(());
                };
                let (first_offset, last_offset) = match &head.state {
                    ProductionDelallocEntryState::Prepared(pending) => {
                        (pending.offset, pending.offset)
                    }
                    ProductionDelallocEntryState::Ready(pending) => {
                        let last = guard
                            .delalloc
                            .production
                            .ready_prefix_end(pending.offset, usize::MAX)
                            .unwrap_or(pending.offset);
                        (pending.offset, last)
                    }
                    ProductionDelallocEntryState::Claimed { certificate, .. } => {
                        let offset = certificate.page_index() * MMArch::PAGE_SIZE;
                        (offset, offset)
                    }
                };
                (
                    guard.page_cache.clone().ok_or(SystemError::EIO)?,
                    first_offset / MMArch::PAGE_SIZE,
                    last_offset / MMArch::PAGE_SIZE,
                )
            };
            page_cache
                .manager()
                .writeback_range(first_page, last_page)?;
        }
    }

    /// Persist only the delayed-allocation prefix required to make one direct
    /// read range coherent with buffered writes.
    ///
    /// The lower mapper is FIFO, so reaching an overlapping entry may require
    /// submitting earlier entries, but entries after the final overlap are
    /// unrelated to this read and remain delayed.
    fn drain_delalloc_range_before_eager(
        &self,
        offset: usize,
        len: usize,
    ) -> Result<(), SystemError> {
        if len == 0 {
            return Ok(());
        }
        let last_byte = offset.checked_add(len - 1).ok_or(SystemError::EOVERFLOW)?;
        let first_offset = offset & !(MMArch::PAGE_SIZE - 1);
        let last_offset = last_byte & !(MMArch::PAGE_SIZE - 1);
        let page_cache = self
            .inner
            .lock()
            .page_cache
            .clone()
            .ok_or(SystemError::EIO)?;

        loop {
            let batch = {
                let guard = self.inner.lock();
                let Some((&target_offset, _)) = guard
                    .delalloc
                    .production
                    .entries
                    .range(first_offset..=last_offset)
                    .next_back()
                else {
                    break;
                };
                let Some((&head_offset, head)) = guard.delalloc.production.head() else {
                    return Err(SystemError::EIO);
                };
                let submit_last = match &head.state {
                    ProductionDelallocEntryState::Prepared(_) => head_offset,
                    ProductionDelallocEntryState::Ready(_) => {
                        let max_entries = target_offset
                            .checked_sub(head_offset)
                            .and_then(|distance| distance.checked_div(MMArch::PAGE_SIZE))
                            .and_then(|pages| pages.checked_add(1))
                            .ok_or(SystemError::EIO)?;
                        guard
                            .delalloc
                            .production
                            .ready_prefix_end(head_offset, max_entries)
                            .unwrap_or(head_offset)
                    }
                    ProductionDelallocEntryState::Claimed { certificate, .. } => certificate
                        .page_index()
                        .checked_mul(MMArch::PAGE_SIZE)
                        .ok_or(SystemError::EOVERFLOW)?,
                };
                (
                    head_offset / MMArch::PAGE_SIZE,
                    submit_last / MMArch::PAGE_SIZE,
                )
            };
            page_cache.manager().writeback_range(batch.0, batch.1)?;
        }

        // The requested range may also contain eager dirty pages, including
        // on nojournal and secondary writable mounts which never own delayed
        // mapper authority.
        page_cache.manager().writeback_range(
            first_offset / MMArch::PAGE_SIZE,
            last_offset / MMArch::PAGE_SIZE,
        )
    }

    /// Detach an idle delayed head after the lower mount has fail-stopped.
    /// Claimed heads remain owned by their live submission (which strongly
    /// holds the filesystem); Prepared/Ready heads have no other terminal
    /// owner and must be consumed here before filesystem teardown.
    pub(super) fn terminalize_idle_delalloc_after_fail_stop(&self, fs: &Ext4FileSystem) -> bool {
        let (inode_num, pending, owner) =
            {
                let _io = self.io_lock.lock();
                let mut guard = self.inner.lock();
                let inode_num = guard.inner_inode_num;
                if guard.delalloc.production.entries.values().any(|entry| {
                    matches!(entry.state, ProductionDelallocEntryState::Claimed { .. })
                }) {
                    return false;
                }
                let mut pending = Vec::new();
                for (_, entry) in core::mem::take(&mut guard.delalloc.production.entries) {
                    match entry.state {
                        ProductionDelallocEntryState::Prepared(entry)
                        | ProductionDelallocEntryState::Ready(entry) => pending.push(entry),
                        ProductionDelallocEntryState::Claimed { .. } => unreachable!(),
                    }
                }
                let owner = guard.delalloc.production.queue_operation.take();
                (inode_num, pending, owner)
            };
        if let Some(authority) = fs.delalloc_mapper_authority.as_ref() {
            for mut pending in pending {
                let _terminalized = fs
                    .fs
                    .terminalize_delalloc_append_block_authorized_after_fail_stop(
                        authority,
                        &mut pending.reservation,
                    );
            }
            if let Some(mut pool) = self.delalloc_pool.lock().take() {
                let _terminalized = fs
                    .fs
                    .terminalize_delalloc_extent_node_pool_authorized_after_fail_stop(
                        authority, &mut pool,
                    );
            }
        }
        fs.unregister_delalloc_inode(inode_num);
        drop(owner);
        true
    }

    fn quarantine_unexpected_rename_reclaim(
        fs: &Arc<Ext4FileSystem>,
        handle: another_ext4::InodeReclaimHandle,
    ) -> Result<(), SystemError> {
        fs.fail_stop_lifecycle();
        // Never risk a second pending capability on a guessed canonical inode.
        // The fail-stopped mount owns this handle until durable orphan recovery
        // can complete after teardown.
        fs.quarantined_reclaims.lock().push(handle);
        Err(SystemError::EIO)
    }

    fn release_clean_metadata_queue_owner(&self, fs: &Arc<Ext4FileSystem>) {
        if let Some(inode) = self.retention_callback_self.upgrade() {
            fs.release_clean_queued_inode(&inode);
        }
    }

    fn reclaim_with_metadata_contention_retry(
        fs: &Arc<Ext4FileSystem>,
        mut handle: another_ext4::InodeReclaimHandle,
    ) -> Result<(), (another_ext4::Ext4Error, another_ext4::InodeReclaimHandle)> {
        loop {
            let observed = fs.fs.metadata_mutation_generation();
            match fs.fs.reclaim_inode(handle) {
                Ok(()) => return Ok(()),
                Err(failure) => {
                    let (error, returned_handle) = failure.into_parts();
                    if error.code() != another_ext4::ErrCode::EAGAIN {
                        return Err((error, returned_handle));
                    }
                    handle = returned_handle;
                    if fs.wait_metadata_mutation_progress(observed).is_err() {
                        return Err((
                            another_ext4::Ext4Error::new(another_ext4::ErrCode::EIO),
                            handle,
                        ));
                    }
                }
            }
        }
    }

    fn reclaim_temporary_inode(
        fs: &Arc<Ext4FileSystem>,
        parent_inode_num: u32,
        name: &str,
        inode: Arc<LockedExt4Inode>,
    ) -> Result<(), SystemError> {
        let lifecycle = inode.lifecycle().clone();
        let _link_mutation = lifecycle.lock_link_mutation();
        let tombstone = fs.begin_freeing(&inode)?;
        let _reuse = fs.begin_reclaim();
        let handle = loop {
            let observed = fs.fs.metadata_mutation_generation();
            match fs.fs.unlink(parent_inode_num, name) {
                Ok(Some(handle)) => break handle,
                Ok(None) => {
                    let error = SystemError::EIO;
                    let _ = fs.poison_freeing(tombstone, error.clone());
                    return Err(error);
                }
                Err(error) if error.code() == another_ext4::ErrCode::EAGAIN => {
                    if let Err(error) = fs.wait_metadata_mutation_progress(observed) {
                        let _ = fs.poison_freeing(tombstone, error.clone());
                        return Err(error);
                    }
                }
                Err(error) => {
                    let error = SystemError::from(error);
                    let _ = fs.poison_freeing(tombstone, error.clone());
                    return Err(error);
                }
            }
        };
        if let Err((error, handle)) = Self::reclaim_with_metadata_contention_retry(fs, handle) {
            *inode.pending_reclaim.lock() = Some(handle);
            let error = SystemError::from(error);
            let _ = fs.poison_freeing(tombstone, error.clone());
            return Err(error);
        }
        fs.complete_freeing(tombstone)
    }

    #[inline]
    fn begin_operation(&self) -> Result<Ext4InodeOperation, SystemError> {
        self.lifecycle.begin_operation()
    }

    pub(super) fn lifecycle(&self) -> &Arc<Ext4InodeLifecycle> {
        &self.lifecycle
    }

    /// 更新 rename 后的缓存
    fn update_rename_cache(
        &self,
        target: &Arc<LockedExt4Inode>,
        src_dir: u32,
        dst_dir: u32,
        old_dname: &DName,
        new_dname: &DName,
        had_dst: bool,
    ) {
        if src_dir == dst_dir {
            let mut guard = self.inner.lock();
            if had_dst {
                guard.children.remove(new_dname);
            }
            if let Some(child) = guard.children.remove(old_dname) {
                child.inner.lock().dname = new_dname.clone();
                guard.children.insert(new_dname.clone(), child);
            }
        } else {
            let (mut src_guard, mut dst_guard) = if src_dir < dst_dir {
                (self.inner.lock(), target.inner.lock())
            } else {
                let d = target.inner.lock();
                let s = self.inner.lock();
                (s, d)
            };

            if had_dst {
                dst_guard.children.remove(new_dname);
            }
            if let Some(child) = src_guard.children.remove(old_dname) {
                dst_guard.children.insert(new_dname.clone(), child.clone());
                drop(src_guard);
                drop(dst_guard);
                let mut child_guard = child.inner.lock();
                child_guard.dname = new_dname.clone();
                child_guard.parent = Arc::downgrade(target);
            }
        }
    }

    /// 更新 exchange 后的缓存：交换两个条目
    fn update_exchange_cache(
        &self,
        target: &Arc<LockedExt4Inode>,
        src_dir: u32,
        dst_dir: u32,
        old_dname: &DName,
        new_dname: &DName,
    ) {
        if src_dir == dst_dir {
            // 同目录交换
            let mut guard = self.inner.lock();
            let old_child = guard.children.remove(old_dname);
            let new_child = guard.children.remove(new_dname);

            if let Some(child) = old_child {
                child.inner.lock().dname = new_dname.clone();
                guard.children.insert(new_dname.clone(), child);
            }
            if let Some(child) = new_child {
                child.inner.lock().dname = old_dname.clone();
                guard.children.insert(old_dname.clone(), child);
            }
        } else {
            // 跨目录交换
            let (mut src_guard, mut dst_guard) = if src_dir < dst_dir {
                (self.inner.lock(), target.inner.lock())
            } else {
                let d = target.inner.lock();
                let s = self.inner.lock();
                (s, d)
            };

            let old_child = src_guard.children.remove(old_dname);
            let new_child = dst_guard.children.remove(new_dname);

            // old_child 移到 target 目录
            if let Some(child) = old_child {
                dst_guard.children.insert(new_dname.clone(), child.clone());
                drop(src_guard);
                drop(dst_guard);

                let mut child_guard = child.inner.lock();
                child_guard.dname = new_dname.clone();
                child_guard.parent = Arc::downgrade(target);
                drop(child_guard);

                // 重新获取锁处理 new_child
                if let Some(new_c) = new_child {
                    let mut src_guard = self.inner.lock();
                    src_guard.children.insert(old_dname.clone(), new_c.clone());
                    drop(src_guard);

                    let mut new_c_guard = new_c.inner.lock();
                    new_c_guard.dname = old_dname.clone();
                    new_c_guard.parent = self.inner.lock().self_ref.clone();
                }
            } else if let Some(new_c) = new_child {
                // 只有 new_child 在缓存中
                src_guard.children.insert(old_dname.clone(), new_c.clone());
                drop(src_guard);
                drop(dst_guard);

                let mut new_c_guard = new_c.inner.lock();
                new_c_guard.dname = old_dname.clone();
                new_c_guard.parent = self.inner.lock().self_ref.clone();
            }
        }
    }

    pub fn new(
        inode_num: u32,
        fs_ptr: Weak<super::filesystem::Ext4FileSystem>,
        dname: DName,
        parent: Option<Weak<LockedExt4Inode>>,
    ) -> Result<Arc<Self>, SystemError> {
        let fs = fs_ptr.upgrade().ok_or(SystemError::EIO)?;
        let attr = fs.fs.getattr(inode_num)?;
        Self::new_with_attr(inode_num, fs_ptr, dname, parent, &attr)
    }

    pub(super) fn new_with_attr(
        inode_num: u32,
        fs_ptr: Weak<super::filesystem::Ext4FileSystem>,
        dname: DName,
        parent: Option<Weak<LockedExt4Inode>>,
        attr: &another_ext4::FileAttr,
    ) -> Result<Arc<Self>, SystemError> {
        debug_assert_eq!(inode_num, attr.ino);
        let fs = fs_ptr.upgrade().ok_or(SystemError::EIO)?;
        let lifecycle = Ext4InodeLifecycle::new();
        let inode = Arc::new_cyclic(|self_ref| LockedExt4Inode {
            inner: Mutex::new(Ext4Inode::new(
                inode_num,
                fs_ptr.clone(),
                dname,
                parent,
                Ext4InodeTimes::from(attr),
            )),
            io_lock: Mutex::new(()),
            metadata_commit_lock: Mutex::new(()),
            size_lock: RwSem::new(()),
            namespace_lock: Mutex::new(()),
            lifecycle,
            retention: InodeRetentionState::new(),
            pending_reclaim: SpinLock::new(None),
            eviction_scheduled: SpinLock::new(false),
            retention_callback_self: self_ref.clone(),
            eviction_filesystem: SpinLock::new(fs_ptr.clone()),
            delalloc_progress: Ext4DelallocProgress::new(),
            delalloc_pool: Mutex::new(None),
        });
        let mut guard = inode.inner.lock();

        // 设置self_ref
        guard.self_ref = Arc::downgrade(&inode);
        guard.cached_file_size = Some(attr.size);

        // Preserve the established eager backend until the delayed-allocation
        // protocol can split ext4's `io_lock`-protected claim/bind phase from
        // PageCache snapshotting. `snapshot_writeback_batch()` calls
        // `mkclean_page()`, which takes an AddressSpace read lock; a shared
        // mmap fault already holds AddressSpace write while it enters
        // `prepare_mmap_write()` and takes `io_lock`. Holding `io_lock` across
        // the current generic admission callback would therefore form an
        // ABBA. Do not add an ext4 admission wrapper here until that split,
        // token lifecycle, and defer/progress protocol are complete.
        let backend: Arc<dyn PageCacheBackend> =
            if attr.ftype == FileType::RegularFile && fs.delalloc_mapper_authority.is_some() {
                Arc::new(Ext4PageCacheBackend::new(Arc::downgrade(&inode)))
            } else {
                Arc::new(AsyncPageCacheBackend::new(
                    Arc::downgrade(&inode) as Weak<dyn IndexNode>
                ))
            };
        let page_cache = PageCache::new_file(
            Arc::downgrade(&inode) as Weak<dyn IndexNode>,
            backend,
            &fs.writeback_domain,
        )?;
        guard.page_cache = Some(page_cache);

        // 对于 FIFO，创建 pipe inode
        if attr.ftype == FileType::Fifo {
            let pipe_inode = LockedPipeInode::new();
            pipe_inode.set_fifo();
            guard.special_node = Some(SpecialNodeData::Pipe(pipe_inode));
        }

        drop(guard);
        Ok(inode)
    }

    fn file_type(ftype: FileType) -> vfs::FileType {
        match ftype {
            FileType::RegularFile => vfs::FileType::File,
            FileType::Directory => vfs::FileType::Dir,
            FileType::CharacterDev => vfs::FileType::CharDevice,
            FileType::BlockDev => vfs::FileType::BlockDevice,
            FileType::Fifo => vfs::FileType::Pipe,
            FileType::Socket => vfs::FileType::Socket,
            FileType::SymLink => vfs::FileType::SymLink,
            _ => {
                log::warn!("Unknown file type, going to treat it as a file");
                vfs::FileType::File
            }
        }
    }
}

impl Ext4Inode {
    fn concret_fs(&self) -> Arc<Ext4FileSystem> {
        self.fs_ptr
            .upgrade()
            .expect("Ext4FileSystem should be alive")
    }

    pub(super) fn new(
        inode_num: u32,
        fs_ptr: Weak<Ext4FileSystem>,
        dname: DName,
        parent: Option<Weak<LockedExt4Inode>>,
        times: Ext4InodeTimes,
    ) -> Self {
        Self {
            inner_inode_num: inode_num,
            fs_ptr,
            page_cache: None,
            children: BTreeMap::new(),
            dname,
            vfs_inode_id: generate_inode_id(),
            parent: parent.unwrap_or_default(),
            self_ref: Weak::new(), // 将在LockedExt4Inode::new()中设置
            special_node: None,
            cached_file_size: None,
            cached_times: times,
            cached_atime_version: 0,
            cached_mtime_version: 0,
            cached_ctime_version: 0,
            durable_atime_version: 0,
            durable_mtime_version: 0,
            durable_ctime_version: 0,
            delalloc: DelallocInodeState::default(),
            dirty_state: InodeDirtyState::empty(),
        }
    }

    /// Construct the bootstrap root inode used while its filesystem object is
    /// still being formed by `Arc::new_cyclic`.
    ///
    /// Keeping this special construction here prevents sibling modules from
    /// depending on private inode-internal state such as the delayed
    /// allocation planner.  The root has no filesystem back-pointer until
    /// publication, and is its own namespace parent, matching the previous
    /// explicit construction.
    pub(super) fn new_mount_root(self_ref: Weak<LockedExt4Inode>, times: Ext4InodeTimes) -> Self {
        let mut inode = Self::new(
            another_ext4::EXT4_ROOT_INO,
            Weak::new(),
            DName::from("/"),
            Some(self_ref.clone()),
            times,
        );
        inode.self_ref = self_ref;
        inode
    }
}

impl LockedExt4Inode {
    /// Transfer the authoritative result of a namespace transaction to this
    /// canonical inode lifetime. `None` means another hard link remains;
    /// `Some` is the unique capability for the zero-link orphan.
    fn handoff_namespace_reclaim(
        self: &Arc<Self>,
        reclaim: Option<another_ext4::InodeReclaimHandle>,
    ) -> Result<(), SystemError> {
        let Some(handle) = reclaim else {
            return Ok(());
        };
        let (fs, inode_num) = {
            let inner = self.inner.lock();
            (inner.concret_fs(), inner.inner_inode_num)
        };
        if handle.inode_id() != inode_num {
            // Never attach a capability to the wrong canonical lifetime. The
            // fail-stopped mount retains it for durable orphan recovery.
            fs.fail_stop_lifecycle();
            fs.quarantined_reclaims.lock().push(handle);
            return Err(SystemError::EIO);
        }
        self.defer_reclaim(handle)
    }

    /// Publish the one-shot capability produced by the final unlink. Physical
    /// reclaim waits until every semantic VFS owner has released this inode.
    pub(super) fn defer_reclaim(
        self: &Arc<Self>,
        handle: another_ext4::InodeReclaimHandle,
    ) -> Result<(), SystemError> {
        let mut pending = self.pending_reclaim.lock();
        if pending.is_some() {
            return Err(SystemError::EIO);
        }
        *pending = Some(handle);
        drop(pending);
        self.try_schedule_deferred_eviction()
    }

    fn cancel_deferred_reclaim_after_relink(&self) {
        // Dropping the capability is the in-memory counterpart of the durable
        // orphan-del transaction. A queued eviction, if any, observes None and
        // cleanly aborts instead of treating cancellation as corruption.
        let _ = self.pending_reclaim.lock().take();
    }

    fn try_schedule_deferred_eviction(self: &Arc<Self>) -> Result<(), SystemError> {
        if self.pending_reclaim.lock().is_none() {
            return Ok(());
        }
        let mut scheduled = self.eviction_scheduled.lock();
        if *scheduled {
            return Ok(());
        }
        if self.retention.try_begin_freeing().is_err() {
            return Ok(());
        }
        *scheduled = true;
        let fs = match self.eviction_filesystem.lock().upgrade() {
            Some(fs) => fs,
            None => {
                *scheduled = false;
                self.retention.abort_freeing();
                return Err(SystemError::ESTALE);
            }
        };
        if let Err(error) = fs.schedule_inode_eviction(self.clone()) {
            *scheduled = false;
            self.retention.abort_freeing();
            return Err(error);
        }
        Ok(())
    }

    pub(super) fn run_deferred_eviction(self: &Arc<Self>) -> Result<(), SystemError> {
        // Final reclaim is also a size/extent publisher.  Close the narrow
        // delayed-append front and materialise its exact head before taking
        // the lifecycle to Freeing or discarding PageCache state.
        let _delalloc_admission = self.close_production_delalloc_admission()?;
        self.drain_delalloc_before_eager()?;
        let fs = self.inner.lock().concret_fs();
        let tombstone = match fs.begin_freeing(self) {
            Ok(tombstone) => tombstone,
            Err(error) => {
                *self.eviction_scheduled.lock() = false;
                self.retention.abort_freeing();
                return Err(error);
            }
        };
        // Serialize the final capability decision with final unlink/relink.
        // `begin_freeing` first closes operation admission and drains existing
        // operations, so this ordering cannot deadlock a relink that already
        // owns the link-mutation lock.
        let _link_mutation = self.lifecycle().lock_link_mutation();
        let handle = match self.pending_reclaim.lock().take() {
            Some(handle) => handle,
            None => {
                fs.abort_freeing(tombstone)?;
                *self.eviction_scheduled.lock() = false;
                self.retention.abort_freeing();
                return Ok(());
            }
        };
        let _reuse = fs.begin_reclaim();
        if let Some(page_cache) = self.page_cache() {
            if let Err(error) = page_cache.truncate(0) {
                *self.pending_reclaim.lock() = Some(handle);
                let _ = fs.poison_freeing(tombstone, error.clone());
                return Err(error);
            }
        }
        match Self::reclaim_with_metadata_contention_retry(&fs, handle) {
            Ok(()) => {
                fs.complete_freeing(tombstone)?;
                Ok(())
            }
            Err((error, handle)) => {
                *self.pending_reclaim.lock() = Some(handle);
                let error = SystemError::from(error);
                let _ = fs.poison_freeing(tombstone, error.clone());
                Err(error)
            }
        }
    }

    fn freeze_metadata(&self, datasync: bool) -> Result<Ext4FrozenMetadata, SystemError> {
        let _size = self.size_lock.read();
        let _io = self.io_lock.lock();
        let guard = self.inner.lock();
        let mut dirty = guard
            .dirty_state
            .intersection(InodeDirtyState::PERSISTENT_DIRTY);
        if datasync {
            dirty.remove(
                InodeDirtyState::ATIME_DIRTY
                    | InodeDirtyState::MTIME_DIRTY
                    | InodeDirtyState::CTIME_DIRTY,
            );
        }
        Ok(Ext4FrozenMetadata {
            fs: guard.concret_fs(),
            inode_num: guard.inner_inode_num,
            dirty,
            cached_size: guard.cached_file_size,
            cached_times: guard.cached_times,
            atime_version: guard.cached_atime_version,
            mtime_version: guard.cached_mtime_version,
            ctime_version: guard.cached_ctime_version,
        })
    }

    fn flush_frozen_metadata(&self, snapshot: Ext4FrozenMetadata) -> Result<(), SystemError> {
        let _io = self.io_lock.lock();
        let _metadata_commit = self.metadata_commit_lock.lock();
        let (current_size, durable_atime_version, durable_mtime_version, durable_ctime_version) = {
            let guard = self.inner.lock();
            (
                guard.cached_file_size,
                guard.durable_atime_version,
                guard.durable_mtime_version,
                guard.durable_ctime_version,
            )
        };

        // Cached SIZE_DIRTY is growth-only. A concurrent truncate commits
        // synchronously and leaves a smaller cached size; never resurrect the
        // frozen pre-truncate EOF. Conversely, a later growth may coexist with
        // this fsync, so the lower layer applies this size as a lower bound.
        let size = snapshot
            .dirty
            .contains(InodeDirtyState::SIZE_DIRTY)
            .then_some(snapshot.cached_size)
            .flatten()
            .filter(|frozen| current_size.is_some_and(|current| current >= *frozen));
        let atime = (snapshot.dirty.contains(InodeDirtyState::ATIME_DIRTY)
            && durable_atime_version < snapshot.atime_version)
            .then_some(snapshot.cached_times.atime);
        let mtime = (snapshot.dirty.contains(InodeDirtyState::MTIME_DIRTY)
            && durable_mtime_version < snapshot.mtime_version)
            .then_some(snapshot.cached_times.mtime);
        let ctime = (snapshot.dirty.contains(InodeDirtyState::CTIME_DIRTY)
            && durable_ctime_version < snapshot.ctime_version)
            .then_some(snapshot.cached_times.ctime);

        if size.is_some() || atime.is_some() || mtime.is_some() || ctime.is_some() {
            snapshot.fs.retry_metadata_contention(|| {
                snapshot
                    .fs
                    .fs
                    .commit_inode_metadata(snapshot.inode_num, size, atime, mtime, ctime)
            })?;
        }

        let mut guard = self.inner.lock();
        if atime.is_some() {
            guard.durable_atime_version = guard.durable_atime_version.max(snapshot.atime_version);
        }
        if mtime.is_some() {
            guard.durable_mtime_version = guard.durable_mtime_version.max(snapshot.mtime_version);
        }
        if ctime.is_some() {
            guard.durable_ctime_version = guard.durable_ctime_version.max(snapshot.ctime_version);
        }
        if size.is_some() && guard.cached_file_size == snapshot.cached_size {
            guard.dirty_state.remove(InodeDirtyState::SIZE_DIRTY);
        }
        if atime.is_some() && guard.cached_atime_version == snapshot.atime_version {
            guard.dirty_state.remove(InodeDirtyState::ATIME_DIRTY);
        }
        if mtime.is_some() && guard.cached_mtime_version == snapshot.mtime_version {
            guard.dirty_state.remove(InodeDirtyState::MTIME_DIRTY);
        }
        if ctime.is_some() && guard.cached_ctime_version == snapshot.ctime_version {
            guard.dirty_state.remove(InodeDirtyState::CTIME_DIRTY);
        }
        drop(guard);
        self.release_clean_metadata_queue_owner(&snapshot.fs);
        Ok(())
    }

    pub(super) fn flush_metadata(&self, datasync: bool) -> Result<(), SystemError> {
        let _operation = self.begin_operation()?;
        let _delalloc_admission = self.close_production_delalloc_admission()?;
        self.drain_delalloc_before_eager()?;
        let _io_guard = self.io_lock.lock();
        let _metadata_commit = self.metadata_commit_lock.lock();
        let (
            fs,
            inode_num,
            dirty,
            cached_size,
            cached_times,
            cached_atime_version,
            cached_mtime_version,
            cached_ctime_version,
        ) = {
            let guard = self.inner.lock();
            (
                guard.concret_fs(),
                guard.inner_inode_num,
                guard.dirty_state,
                guard.cached_file_size,
                guard.cached_times,
                guard.cached_atime_version,
                guard.cached_mtime_version,
                guard.cached_ctime_version,
            )
        };

        let size_dirty = dirty.contains(InodeDirtyState::SIZE_DIRTY);
        let atime_dirty = dirty.contains(InodeDirtyState::ATIME_DIRTY);
        let mtime_dirty = dirty.contains(InodeDirtyState::MTIME_DIRTY);
        let ctime_dirty = dirty.contains(InodeDirtyState::CTIME_DIRTY);

        if !size_dirty && (datasync || (!atime_dirty && !mtime_dirty && !ctime_dirty)) {
            self.release_clean_metadata_queue_owner(&fs);
            return Ok(());
        }

        let size = if size_dirty {
            Some(match cached_size {
                Some(size) => size,
                None => fs.fs.getattr(inode_num)?.size,
            })
        } else {
            None
        };
        let atime = if !datasync && atime_dirty {
            Some(cached_times.atime)
        } else {
            None
        };
        let mtime = if !datasync && mtime_dirty {
            Some(cached_times.mtime)
        } else {
            None
        };
        let ctime = if !datasync && ctime_dirty {
            Some(cached_times.ctime)
        } else {
            None
        };
        fs.retry_metadata_contention(|| {
            fs.fs
                .commit_inode_metadata(inode_num, size, atime, mtime, ctime)
        })?;

        let mut guard = self.inner.lock();
        if !datasync && atime_dirty {
            guard.durable_atime_version = guard.durable_atime_version.max(cached_atime_version);
        }
        if !datasync && mtime_dirty {
            guard.durable_mtime_version = guard.durable_mtime_version.max(cached_mtime_version);
        }
        if !datasync && ctime_dirty {
            guard.durable_ctime_version = guard.durable_ctime_version.max(cached_ctime_version);
        }
        if size_dirty && guard.cached_file_size == cached_size {
            guard.dirty_state.remove(InodeDirtyState::SIZE_DIRTY);
        }
        if !datasync && atime_dirty && guard.cached_atime_version == cached_atime_version {
            guard.dirty_state.remove(InodeDirtyState::ATIME_DIRTY);
        }
        if !datasync && mtime_dirty && guard.cached_mtime_version == cached_mtime_version {
            guard.dirty_state.remove(InodeDirtyState::MTIME_DIRTY);
        }
        if !datasync && ctime_dirty && guard.cached_ctime_version == cached_ctime_version {
            guard.dirty_state.remove(InodeDirtyState::CTIME_DIRTY);
        }
        drop(guard);
        self.release_clean_metadata_queue_owner(&fs);
        Ok(())
    }

    /// Prepare the on-disk extent before a shared file VMA becomes writable.
    ///
    /// This is the ext4 counterpart of Linux `ext4_page_mkwrite()`: page-cache
    /// dirtying alone is insufficient for a sparse page because writeback uses
    /// `write_data_only()` and therefore requires the backing block to exist.
    pub(super) fn prepare_mmap_write(
        &self,
        page_index: usize,
    ) -> Result<Ext4MmapWriteGuard<'_>, SystemError> {
        let operation = self.begin_operation()?;
        let _delalloc_admission = self.close_production_delalloc_admission()?;
        self.drain_delalloc_before_eager()?;
        let size_guard = self.size_lock.read();
        let io_guard = self.io_lock.lock();
        let _metadata_commit = self.metadata_commit_lock.lock();
        let (fs, inode_num, file_size) = {
            let mut guard = self.inner.lock();
            let fs = guard.concret_fs();
            let file_size = match guard.cached_file_size {
                Some(size) => size,
                None => {
                    let size = fs.fs.getattr(guard.inner_inode_num)?.size;
                    guard.cached_file_size = Some(size);
                    size
                }
            };
            (fs, guard.inner_inode_num, file_size)
        };
        let page_start = page_index
            .checked_mul(MMArch::PAGE_SIZE)
            .ok_or(SystemError::EFBIG)?;
        if page_start >= file_size as usize {
            return Err(SystemError::EFBIG);
        }
        let time = PosixTimeSpec::now().tv_sec.to_u32().unwrap_or(0);
        let (mtime_version, ctime_version) = {
            let guard = self.inner.lock();
            (
                guard
                    .cached_mtime_version
                    .checked_add(1)
                    .ok_or(SystemError::EOVERFLOW)?,
                guard
                    .cached_ctime_version
                    .checked_add(1)
                    .ok_or(SystemError::EOVERFLOW)?,
            )
        };
        fs.retry_metadata_contention(|| {
            fs.fs.prepare_buffered_write(
                inode_num,
                page_start,
                MMArch::PAGE_SIZE,
                file_size,
                Some(time),
            )
        })?;
        // The size read lock remains held through the generic page-cache
        // handoff, so truncate cannot remove the prepared extent.  Release the
        // inode I/O lock first: filemap_page_mkwrite may wait for an existing
        // writeback, whose write_sync path needs this same lock.
        let self_arc = {
            let mut guard = self.inner.lock();
            guard.cached_times.mtime = time;
            guard.cached_times.ctime = time;
            guard.cached_mtime_version = mtime_version;
            guard.cached_ctime_version = ctime_version;
            guard.self_ref.upgrade().ok_or(SystemError::ENOENT)?
        };
        drop(io_guard);
        Ext4FileSystem::mark_inode_dirty(
            &self_arc,
            InodeDirtyState::MTIME_DIRTY | InodeDirtyState::CTIME_DIRTY,
        )?;
        Ok(Ext4MmapWriteGuard {
            _operation: operation,
            _size_guard: size_guard,
        })
    }
}

impl Debug for Ext4Inode {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Ext4Inode")
    }
}

pub(crate) fn run_lifecycle_selftests() -> String {
    let mut failures = 0usize;
    let mut report = String::new();
    let mut append = |name: &str, ok: bool| {
        if ok {
            report.push_str(&format!("{name}=ok\n"));
        } else {
            failures += 1;
            report.push_str(&format!("{name}=fail\n"));
        }
    };

    let lifecycle = Ext4InodeLifecycle::new();
    let operation = lifecycle.begin_operation();
    append("live_operation", operation.is_ok());
    drop(operation);
    append("begin_freeing", lifecycle.begin_freeing().is_ok());
    lifecycle.wait_for_quiescent();
    lifecycle.set_state(Ext4InodeLifecycleState::Retired);
    append(
        "retired_rejects_operation",
        lifecycle.begin_operation().err() == Some(SystemError::ESTALE),
    );

    let lifecycle = Ext4InodeLifecycle::new();
    let outer = lifecycle.begin_operation().expect("live operation");
    append("reentrant_begin_freeing", lifecycle.begin_freeing().is_ok());
    let nested = lifecycle.begin_operation();
    append("freeing_allows_owner_nested_operation", nested.is_ok());
    drop(nested);
    drop(outer);
    lifecycle.wait_for_quiescent();
    append(
        "reentrant_operations_drained",
        lifecycle.inner.lock().active_operations == 0,
    );

    let lifecycle = Ext4InodeLifecycle::new();
    append("abort_begin", lifecycle.begin_freeing().is_ok());
    lifecycle.set_state(Ext4InodeLifecycleState::Live);
    append("abort_restores_live", lifecycle.begin_operation().is_ok());

    let lifecycle = Ext4InodeLifecycle::new();
    lifecycle.set_state(Ext4InodeLifecycleState::Poisoned(SystemError::EIO));
    append(
        "poison_is_observable",
        lifecycle.begin_operation().err() == Some(SystemError::EIO),
    );

    if failures == 0 {
        report.insert_str(0, "status=ok\n");
    } else {
        report.insert_str(0, &format!("status=fail failures={failures}\n"));
    }
    report
}
