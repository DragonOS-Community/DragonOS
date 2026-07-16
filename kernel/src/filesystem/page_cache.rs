use core::{
    mem::ManuallyDrop,
    sync::atomic::{AtomicBool, AtomicU64, AtomicU8, AtomicUsize, Ordering},
};

use alloc::{
    collections::BTreeSet,
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
        page::{page_manager_lock, page_reclaimer_lock, Page, PageFlags},
        ucontext::AddressSpace,
        MemoryManagementArch,
    },
};
use crate::{libs::align::page_align_up, mm::page::PageType};
use lazy_static::lazy_static;

static PAGE_CACHE_ID: AtomicUsize = AtomicUsize::new(0);
static PAGE_CACHE_DMA_RESERVATION_ID: AtomicU64 = AtomicU64::new(1);
static PAGE_CACHE_WRITEBACK_TAG_EPOCH: AtomicU64 = AtomicU64::new(1);

const PAGECACHE_IO_WORKERS: usize = 4;
const MAX_ASYNC_WRITEBACK_BATCHES: usize = PAGECACHE_IO_WORKERS * 2;
static PAGECACHE_IO_RR: AtomicUsize = AtomicUsize::new(0);
static PAGECACHE_WRITEBACK_RR: AtomicUsize = AtomicUsize::new(0);
static ASYNC_WRITEBACK_BATCHES: AtomicUsize = AtomicUsize::new(0);
static ASYNC_WRITEBACK_COMPLETIONS: AtomicU64 = AtomicU64::new(0);
static ASYNC_WRITEBACK_WAIT: WaitQueue = WaitQueue::default();
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
#[repr(u8)]
enum PageEntryAccounting {
    Unaccounted = 0,
    File = 1,
    Shmem = 2,
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

/// Exercise page-cache membership accounting with local identity assertions and
/// a high-signal aggregate check of the production vmstat wiring.
pub(crate) fn run_accounting_debug_selftest() -> Result<alloc::string::String, SystemError> {
    if PAGECACHE_ACCOUNTING_SELFTEST_RUNNING
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return Err(SystemError::EBUSY);
    }
    let _running = PageCacheAccountingSelftestGuard;

    #[allow(dead_code)]
    struct PageEntryLayoutBaseline {
        page: Arc<Page>,
        state: AtomicU8,
        writeback_tag: AtomicU64,
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

    // Loading rollback consumes membership once; a late state publication on
    // the detached entry must not revive it.
    let state_cache = PageCache::new(None, None);
    let loading_page = state_cache.allocate_page(Arc::downgrade(&state_cache), 0)?;
    let loading_entry = Arc::new(PageEntry::new(loading_page.clone(), PageState::Loading));
    state_cache
        .inner
        .lock()
        .insert_entry(0, loading_entry.clone());
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
    drop_cache.inner.lock().insert_entry(0, drop_entry.clone());
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
        "status=ok\nfile_membership=ok\nshmem_membership=ok\ndirty_membership=ok\nwriteback_membership=ok\nunevictable_membership=ok\ninflight_teardown=ok\nlate_completion=ok\nglobal_wiring=ok\nlayout=ok\nfile_drop_drift={file_drop_drift}\nshmem_drop_drift={shmem_drop_drift}\ndirty_drop_drift={dirty_drop_drift}\nwriteback_drop_drift={writeback_drop_drift}\nunevictable_drop_drift={unevictable_drop_drift}\nentry_size={entry_size}\nbaseline_size={baseline_size}\n"
    ))
}

struct AsyncWritebackPermit;

impl AsyncWritebackPermit {
    fn try_acquire() -> Option<Self> {
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

    fn acquire() -> Self {
        ASYNC_WRITEBACK_WAIT.wait_until(Self::try_acquire)
    }
}

impl Drop for AsyncWritebackPermit {
    fn drop(&mut self) {
        ASYNC_WRITEBACK_BATCHES.fetch_sub(1, Ordering::AcqRel);
        ASYNC_WRITEBACK_COMPLETIONS.fetch_add(1, Ordering::AcqRel);
        ASYNC_WRITEBACK_WAIT.wake_all();
    }
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

pub trait PageCacheBackend: Send + Sync + core::fmt::Debug {
    fn read_page(&self, index: usize, buf: &mut [u8]) -> Result<usize, SystemError>;
    fn write_page(&self, index: usize, buf: &[u8]) -> Result<usize, SystemError>;
    fn npages(&self) -> usize;

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

    /// Run the page-cache claim while the filesystem's write admission is
    /// held. Filesystems with an explicit writeback barrier override this
    /// method. The page-cache manager samples i_size only after it has also
    /// acquired the invalidate read lock.
    fn with_write_admission(
        &self,
        claim: &mut dyn FnMut() -> Result<(), SystemError>,
    ) -> Result<(), SystemError> {
        claim()
    }

    /// Try-only admission used by reclaimer workers. Returning `false` means
    /// no page state was changed and the candidate remains Dirty.
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
    reclaimer_writeback_active: AtomicBool,
    manager: PageCacheManager,
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
    /// Aggregated semantic owner for all dirty and writeback pages in this mapping.
    dirty_retention: Option<InodeRetentionGuard>,
    dirty_preparations: usize,
    kind: PageCacheKind,
    page_cache_ref: Weak<PageCache>,
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
    state: AtomicU8,
    writeback_tag: AtomicU64,
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
    file_size: usize,
    entries: Vec<(usize, Arc<PageEntry>, Arc<Page>)>,
    guards: Vec<WritebackGuard>,
    data: Vec<u8>,
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
            inner.insert_entry(item.descriptor.page_index, item.entry.clone());
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

    pub fn prefetch_page(&self, page_index: usize) -> Result<(), SystemError> {
        self.upgrade()?.start_async_read(page_index)
    }

    pub fn update_page(&self, page_index: usize) -> Result<(), SystemError> {
        let cache = self.upgrade()?;
        if let Some(entry) = cache.inner.lock().get_entry(page_index) {
            let state = entry.state();
            if state == PageState::Loading {
                let _ = entry.wait_ready()?;
            }
        }
        cache.mark_page_dirty(page_index)?;
        Ok(())
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
            let dst = unsafe { page.as_slice_mut() };
            dst[page_offset..page_offset + data.len()].copy_from_slice(data);
            page.add_flags(PageFlags::PG_UPTODATE);
            if keep_dirty {
                page.add_flags(PageFlags::PG_DIRTY);
            }
            drop(page);

            if keep_dirty {
                let mut inner = cache.inner.lock();
                let Some(current) = inner.get_entry(page_index) else {
                    return Ok(false);
                };
                if !Arc::ptr_eq(&current, &entry) {
                    return Ok(false);
                }
                match current.state() {
                    PageState::Loading => continue,
                    PageState::Writeback => return Ok(true),
                    PageState::Error => return Ok(false),
                    PageState::UpToDate | PageState::Dirty => {
                        let old_state = current.state();
                        inner.dirty_pages.insert(page_index);
                        current.account_state_transition(old_state, PageState::Dirty);
                        current.set_state(PageState::Dirty);
                    }
                }
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

    fn claim_next_writeback_batch(
        cache: &Arc<PageCache>,
        start_index: usize,
        end_index: usize,
        file_size: usize,
        required_first: Option<(usize, &Arc<PageEntry>, u64)>,
    ) -> Result<Option<ClaimedWritebackBatch>, SystemError> {
        if start_index > end_index {
            return Ok(None);
        }
        let backend = cache.backend();
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

        let first_index = {
            let mut inner = cache.inner.lock();
            if let Some((required_index, required_entry, epoch)) = required_first {
                let Some(current) = inner.pages.get(&required_index) else {
                    return Ok(None);
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
                    return Ok(None);
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
                return Ok(None);
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
                    return Ok(None);
                };
                if !Arc::ptr_eq(current, entry) {
                    return Ok(None);
                }
                if current.state() == PageState::Error {
                    return Err(SystemError::EIO);
                }
            }

            for (page_index, entry) in candidates.drain(..) {
                let old_state = entry.state();
                if old_state == PageState::UpToDate {
                    entry.account_state_transition(PageState::UpToDate, PageState::Dirty);
                    entry.set_state(PageState::Dirty);
                }
                debug_assert_eq!(entry.state(), PageState::Dirty);
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
            first_index
        };

        Ok(Some(ClaimedWritebackBatch {
            cache: cache.clone(),
            backend,
            first_index,
            file_size,
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

    fn snapshot_writeback_batch(batch: &mut ClaimedWritebackBatch) -> Result<(), SystemError> {
        for (page_index, _entry, page) in batch.entries.iter() {
            let page_start = page_index
                .checked_mul(MMArch::PAGE_SIZE)
                .ok_or(SystemError::EOVERFLOW)?;
            let len = batch
                .file_size
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
            page_guard.remove_flags(PageFlags::PG_DIRTY);
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
    fn submit_writeback_batch(batch: ClaimedWritebackBatch) -> Result<(), SystemError> {
        let result = if batch.data.is_empty() {
            Ok(())
        } else if let Some(backend) = batch.backend.as_ref() {
            backend.write_pages(batch.first_index, &batch.data)
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
                    }),
                (Err(error), _) | (_, Err(error)) => Err(error),
            }
        };
        Self::complete_writeback_batch(batch, result)
    }

    fn claim_and_snapshot_with_stable_size(
        cache: &Arc<PageCache>,
        start_index: usize,
        end_index: usize,
        file_size: usize,
    ) -> Result<Option<ClaimedWritebackBatch>, SystemError> {
        let _invalidate = cache.invalidate_read();
        Self::claim_and_snapshot_locked(cache, start_index, end_index, file_size)
    }

    /// Claim and snapshot with the invalidate read lock already held.
    fn claim_and_snapshot_locked(
        cache: &Arc<PageCache>,
        start_index: usize,
        end_index: usize,
        file_size: usize,
    ) -> Result<Option<ClaimedWritebackBatch>, SystemError> {
        let Some(mut batch) =
            Self::claim_next_writeback_batch(cache, start_index, end_index, file_size, None)?
        else {
            return Ok(None);
        };
        if let Err(error) = Self::snapshot_writeback_batch(&mut batch) {
            return Self::complete_writeback_batch(batch, Err(error)).map(|_| None);
        }
        Ok(Some(batch))
    }

    fn claim_and_snapshot_tagged_locked(
        cache: &Arc<PageCache>,
        start_index: usize,
        end_index: usize,
        file_size: usize,
        required_entry: &Arc<PageEntry>,
        epoch: u64,
    ) -> Result<Option<ClaimedWritebackBatch>, SystemError> {
        let Some(mut batch) = Self::claim_next_writeback_batch(
            cache,
            start_index,
            end_index,
            file_size,
            Some((start_index, required_entry, epoch)),
        )?
        else {
            return Ok(None);
        };
        if let Err(error) = Self::snapshot_writeback_batch(&mut batch) {
            return Self::complete_writeback_batch(batch, Err(error)).map(|_| None);
        }
        Ok(Some(batch))
    }

    fn writeback_next_batch_with_stable_size(
        cache: &Arc<PageCache>,
        start_index: usize,
        end_index: usize,
        file_size: usize,
    ) -> Result<bool, SystemError> {
        let Some(batch) =
            Self::claim_and_snapshot_with_stable_size(cache, start_index, end_index, file_size)?
        else {
            return Ok(false);
        };
        Self::submit_writeback_batch(batch)?;
        Ok(true)
    }

    fn claim_next_batch_with_admission(
        &self,
        cache: &Arc<PageCache>,
        inode: &Arc<dyn IndexNode>,
        start_index: usize,
        end_index: usize,
    ) -> Result<Option<ClaimedWritebackBatch>, SystemError> {
        let Some(backend) = cache.backend() else {
            let _invalidate = cache.invalidate_read();
            let file_size = inode.metadata()?.size.max(0) as usize;
            return Self::claim_and_snapshot_locked(cache, start_index, end_index, file_size);
        };
        let mut claimed = None;
        backend.with_write_admission(&mut || {
            let _invalidate = cache.invalidate_read();
            let file_size = backend.stable_writeback_size(inode)?;
            claimed = Self::claim_and_snapshot_locked(cache, start_index, end_index, file_size)?;
            Ok(())
        })?;
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
    ) -> Result<Option<ClaimedWritebackBatch>, SystemError> {
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
            );
        };
        let mut claimed = None;
        backend.with_write_admission(&mut || {
            let _invalidate = cache.invalidate_read();
            let file_size = backend.stable_writeback_size(inode)?;
            claimed = Self::claim_and_snapshot_tagged_locked(
                cache,
                start_index,
                end_index,
                file_size,
                required_entry,
                epoch,
            )?;
            Ok(())
        })?;
        Ok(claimed)
    }

    fn try_claim_next_batch_with_admission(
        &self,
        cache: &Arc<PageCache>,
        inode: &Arc<dyn IndexNode>,
        start_index: usize,
        end_index: usize,
    ) -> Result<Option<ClaimedWritebackBatch>, SystemError> {
        let Some(backend) = cache.backend() else {
            let Some(_invalidate) = cache.try_invalidate_read() else {
                return Ok(None);
            };
            let file_size = inode.metadata()?.size.max(0) as usize;
            return Self::claim_and_snapshot_locked(cache, start_index, end_index, file_size);
        };
        let mut invalidate_acquired = false;
        let mut claimed = None;
        let admitted = backend.try_with_write_admission(&mut || {
            let Some(_invalidate) = cache.try_invalidate_read() else {
                return Ok(());
            };
            invalidate_acquired = true;
            let file_size = backend.stable_writeback_size(inode)?;
            claimed = Self::claim_and_snapshot_locked(cache, start_index, end_index, file_size)?;
            Ok(())
        })?;
        if !admitted || !invalidate_acquired {
            return Ok(None);
        }
        Ok(claimed)
    }

    fn writeback_next_batch(
        &self,
        cache: &Arc<PageCache>,
        inode: &Arc<dyn IndexNode>,
        start_index: usize,
        end_index: usize,
    ) -> Result<bool, SystemError> {
        let Some(batch) =
            self.claim_next_batch_with_admission(cache, inode, start_index, end_index)?
        else {
            return Ok(false);
        };
        Self::submit_writeback_batch(batch)?;
        Ok(true)
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
                    if entry.state() == PageState::Writeback {
                        entries.push(entry.clone());
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
            for entry in entries {
                if let Err(error) = Self::wait_writeback_entry(entry) {
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
            while self.writeback_next_batch(&cache, &inode, start_index, end_index)? {}
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
                        &cache, cursor, end_index, file_size, None,
                    ) {
                        Ok(Some(mut batch)) => {
                            pc_stats::record_invalidation_launder_batch(batch.entries.len());
                            let last_index = batch
                                .entries
                                .last()
                                .map(|(index, _, _)| *index)
                                .unwrap_or(cursor);
                            let snapshot_result = Self::snapshot_writeback_batch(&mut batch);
                            Ok(Some((batch, last_index, snapshot_result)))
                        }
                        Ok(None) => Ok(None),
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
                    Err(error) => Self::complete_writeback_batch(batch, Err(error)),
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

    pub fn prepare_page_mkwrite(
        &self,
        page_index: usize,
        page: &Arc<Page>,
    ) -> Result<(), SystemError> {
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

            {
                page.write().add_flags(PageFlags::PG_DIRTY);
            }

            let mut inner = cache.inner.lock();
            let Some(current) = inner.get_entry(page_index) else {
                return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
            };
            if !Arc::ptr_eq(&current, &entry) || !Arc::ptr_eq(&current.page, page) {
                return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
            }

            match current.state() {
                PageState::Loading | PageState::Writeback => continue,
                PageState::Error => return Err(SystemError::EIO),
                PageState::UpToDate | PageState::Dirty => {
                    let old_state = current.state();
                    inner.dirty_pages.insert(page_index);
                    current.account_state_transition(old_state, PageState::Dirty);
                    current.set_state(PageState::Dirty);
                    return Ok(());
                }
            }
        }
    }

    pub fn start_writeback_range(
        &self,
        start_index: usize,
        end_index: usize,
        sync_all: bool,
    ) -> Result<(), SystemError> {
        if start_index > end_index {
            return Ok(());
        }
        let cache = self.upgrade()?;
        let inode = cache
            .inode()
            .and_then(|inode| inode.upgrade())
            .ok_or(SystemError::EIO)?;
        let _tagged_writeback = cache.tagged_writeback_lock.lock();

        // Freeze the caller-visible dirty set with an epoch tag, equivalent to
        // Linux's PAGECACHE_TAG_TOWRITE pass but without materializing an
        // unbounded Vec. A monotonic index walk bounds transient memory and
        // prevents a low-index redirtier from starving later pages.
        let mut epoch = PAGE_CACHE_WRITEBACK_TAG_EPOCH.fetch_add(1, Ordering::AcqRel);
        if epoch == 0 {
            epoch = PAGE_CACHE_WRITEBACK_TAG_EPOCH.fetch_add(1, Ordering::AcqRel);
        }
        const WRITEBACK_TAG_CHUNK: usize = 256;
        let Some(frozen_end) = ({
            let inner = cache.inner.lock();
            inner
                .dirty_pages
                .range(start_index..=end_index)
                .next_back()
                .copied()
        }) else {
            return Ok(());
        };
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
                        entry.set_writeback_tag(epoch);
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
            // Mirror Linux tag_pages_for_writeback(): do not monopolize the
            // page-cache index lock or CPU while tagging a very large range.
            crate::sched::sched_yield();
        }

        let mut cursor = start_index;
        loop {
            if cursor > frozen_end {
                break;
            }
            let (target, last_scanned) = {
                let inner = cache.inner.lock();
                let mut target = None;
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
                    if entry.writeback_tag() == epoch {
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
                        target = Some((*index, entry.clone(), tagged_end));
                        break;
                    }
                }
                (target, last_scanned)
            };
            let Some((target_index, target_entry, tagged_end)) = target else {
                let Some(last_scanned) = last_scanned else {
                    break;
                };
                if last_scanned >= frozen_end || last_scanned == usize::MAX {
                    break;
                }
                cursor = last_scanned + 1;
                crate::sched::sched_yield();
                continue;
            };

            if target_entry.state() == PageState::Writeback {
                if sync_all {
                    Self::wait_writeback_entry(target_entry.clone())?;
                    continue;
                } else {
                    if target_index == usize::MAX {
                        break;
                    }
                    cursor = target_index + 1;
                    continue;
                }
            }

            let permit = AsyncWritebackPermit::acquire();
            let Some(batch) = self.claim_tagged_batch_with_admission(
                &cache,
                &inode,
                target_index,
                tagged_end,
                &target_entry,
                epoch,
            )?
            else {
                drop(permit);
                let (same_tagged_entry, state) = {
                    let inner = cache.inner.lock();
                    match inner.pages.get(&target_index) {
                        Some(current)
                            if Arc::ptr_eq(current, &target_entry)
                                && inner.dirty_pages.contains(&target_index)
                                && current.writeback_tag() == epoch =>
                        {
                            (true, current.state())
                        }
                        _ => (false, target_entry.state()),
                    }
                };
                if sync_all && same_tagged_entry {
                    if state == PageState::Writeback {
                        Self::wait_writeback_entry(target_entry.clone())?;
                    }
                    // Dirty may have raced with another completion between
                    // admission and claim. Retry the same tagged target; only
                    // identity removal or loss of the epoch permits advance.
                    continue;
                }
                if target_index == usize::MAX {
                    break;
                }
                cursor = target_index + 1;
                continue;
            };
            let last_index = batch
                .entries
                .last()
                .map(|(index, _, _)| *index)
                .unwrap_or(target_index);
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
        }
        Ok(())
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
                let Ok(Some(batch)) =
                    manager.try_claim_next_batch_with_admission(&cache, &inode, cursor, end_index)
                else {
                    break;
                };
                let Some(last_index) = batch.entries.last().map(|(index, _, _)| *index) else {
                    break;
                };
                if Self::submit_writeback_batch(batch).is_err() || last_index == usize::MAX {
                    break;
                }
                cursor = last_index + 1;
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

    pub fn remove_page(&self, page_index: usize) -> Result<Option<Arc<Page>>, SystemError> {
        let cache = self.upgrade()?;
        let removed = cache.lock().remove_page(page_index);
        drop(cache.detach_dirty_retention_if_idle());
        Ok(removed)
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
        Self {
            page,
            state: AtomicU8::new(state as u8),
            writeback_tag: AtomicU64::new(0),
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
    fn new(page_cache_ref: Weak<PageCache>, id: usize, kind: PageCacheKind) -> InnerPageCache {
        Self {
            id,
            pages: HashMap::new(),
            page_indices: BTreeSet::new(),
            dirty_pages: BTreeSet::new(),
            writeback_pages: BTreeSet::new(),
            dirty_retention: None,
            dirty_preparations: 0,
            kind,
            page_cache_ref,
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
        Some(entry.page.clone())
    }

    fn get_entry(&self, offset: usize) -> Option<Arc<PageEntry>> {
        self.pages.get(&offset).cloned()
    }

    fn insert_entry(&mut self, offset: usize, entry: Arc<PageEntry>) {
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
        self.page_indices.insert(offset);
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
        let cache = Arc::new_cyclic(|weak| Self {
            id,
            inner: Mutex::new(InnerPageCache::new(weak.clone(), id, kind)),
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

    /// Remove cached pages while the caller holds `invalidate_write()`.
    ///
    /// Filesystems that must serialize their on-disk size update with page
    /// invalidation use this after unmapping PTEs.  Callers must repeat the
    /// unmap-and-lock sequence when this returns `false`.
    pub(crate) fn truncate_locked(&self, new_size: usize) -> Result<bool, SystemError> {
        let first_full_truncate_page = page_align_up(new_size) >> MMArch::PAGE_SHIFT;
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
                        // taking inner, then revalidate every mutable entry
                        // property under the membership lock.
                        drop(page_guard);
                        let mut guard = self.inner.lock();
                        let Some(current) = guard.get_entry(page_index) else {
                            break;
                        };
                        if !Arc::ptr_eq(&current, &entry) {
                            continue;
                        }
                        if current.active_users() != 0 {
                            drop(guard);
                            current.wait_inactive();
                            continue;
                        }
                        match current.state() {
                            PageState::Loading => {
                                drop(guard);
                                let _ = current.wait_ready();
                                continue;
                            }
                            PageState::Writeback => {
                                drop(guard);
                                let _ = current.wait_queue.wait_until(|| match current.state() {
                                    PageState::Writeback => None,
                                    PageState::Error => Some(Err(SystemError::EIO)),
                                    _ => Some(Ok(())),
                                });
                                continue;
                            }
                            _ => guard.remove_page(page_index),
                        }
                    }
                };

                if retry_after_unmap {
                    return Ok(false);
                }

                if let Some(page) = removed_page {
                    self.discard_unlinked_page(&page);
                }
                drop(self.detach_dirty_retention_if_idle());
                break;
            }
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
                return Ok(entry);
            }
            if state == PageState::Error {
                return Err(SystemError::EIO);
            }
            let _ = entry.wait_ready()?;
            return Ok(entry);
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
                    guard.insert_entry(page_index, entry.clone());
                    (entry, true)
                }
            }
        };

        if !need_populate {
            let state = entry.state();
            if state.is_ready() {
                return Ok(entry);
            }
            if state == PageState::Error {
                return Err(SystemError::EIO);
            }
            let _ = entry.wait_ready()?;
            return Ok(entry);
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
                Ok(entry)
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
            guard.insert_entry(page_index, entry.clone());
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
                    guard.insert_entry(page_index, entry.clone());
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
                    guard.insert_entry(page_index, entry.clone());
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

    pub fn mark_page_dirty(&self, page_index: usize) -> Result<(), SystemError> {
        let mut guard = self.inner.lock();
        if let Some(entry) = guard.get_entry(page_index) {
            self.ensure_dirty_retention_locked(&mut guard)?;
            let old_state = entry.state();
            guard.dirty_pages.insert(page_index);
            if old_state == PageState::Writeback {
                return Ok(());
            }
            entry.account_state_transition(old_state, PageState::Dirty);
            entry.set_state(PageState::Dirty);
            return Ok(());
        }
        drop(guard);
        drop(self.detach_dirty_retention_if_idle());
        Ok(())
    }

    pub fn mark_page_dirty_prepared(
        &self,
        page_index: usize,
        reservation: &mut PageDirtyReservation,
    ) -> Result<(), SystemError> {
        assert!(reservation.active);
        let mut guard = self.inner.lock();
        assert!(guard.dirty_preparations != 0);
        guard.dirty_preparations -= 1;
        reservation.active = false;
        if let Some(entry) = guard.get_entry(page_index) {
            let old_state = entry.state();
            guard.dirty_pages.insert(page_index);
            if old_state != PageState::Writeback {
                entry.account_state_transition(old_state, PageState::Dirty);
                entry.set_state(PageState::Dirty);
            }
            return Ok(());
        }
        drop(guard);
        drop(self.detach_dirty_retention_if_idle());
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
        guard.insert_entry(page_index, entry);
        Ok(())
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
            drop(page_guard);
            if let Some(mut reservation) = dirty_reservation.take() {
                self.mark_page_dirty_prepared(item.page_index, &mut reservation)?;
            } else {
                self.mark_page_dirty(item.page_index)?;
            }
        }
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
        drop(page_guards);

        for (index, item) in copies.into_iter().enumerate() {
            if index == 0 {
                self.mark_page_dirty_prepared(item.page_index, &mut dirty_reservation)?;
            } else {
                self.mark_page_dirty(item.page_index)?;
            }
        }

        Ok(ret)
    }
}
