use core::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, AtomicUsize, Ordering};

use alloc::{
    collections::BTreeSet,
    sync::{Arc, Weak},
    vec::Vec,
};
use hashbrown::HashMap;
use system_error::SystemError;

use super::vfs::{
    mount::record_writeback_error_for_fs, FilePrivateData, IndexNode, WritebackControl,
};
use crate::exception::workqueue::{schedule_work, Work, WorkQueue};
use crate::libs::errseq::{ErrSeq, ErrSeqValue};
use crate::libs::mutex::MutexGuard;
use crate::libs::rwsem::{RwSem, RwSemReadGuard, RwSemWriteGuard};
use crate::libs::spinlock::SpinLock;
use crate::libs::wait_queue::WaitQueue;
use crate::mm::page::FileMapInfo;
use crate::mm::page_cache_stats as pc_stats;
use crate::mm::ucontext::LockedVMA;
use crate::sched::completion::Completion;
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

const PAGECACHE_IO_WORKERS: usize = 4;
static PAGECACHE_IO_RR: AtomicUsize = AtomicUsize::new(0);

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
    static ref PAGECACHE_REGISTRY: SpinLock<Vec<Weak<PageCache>>> = SpinLock::new(Vec::new());
}

fn schedule_pagecache_io(work: Arc<Work>) {
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

pub trait PageCacheBackend: Send + Sync + core::fmt::Debug {
    fn read_page(&self, index: usize, buf: &mut [u8]) -> Result<usize, SystemError>;
    fn write_page(&self, index: usize, buf: &[u8]) -> Result<usize, SystemError>;
    fn npages(&self) -> usize;

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
        schedule_pagecache_io(work);
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
    is_shmem: AtomicBool,
    reclassify_lock: Mutex<()>,
    manager: PageCacheManager,
}

#[derive(Debug)]
pub struct InnerPageCache {
    #[allow(unused)]
    id: usize,
    pages: HashMap<usize, Arc<PageEntry>>,
    page_indices: BTreeSet<usize>,
    dirty_pages: BTreeSet<usize>,
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
    accounted_unevictable: AtomicBool,
    active_users: AtomicUsize,
    wait_queue: WaitQueue,
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

    pub fn commit_overwrite(&self, page_index: usize) -> Result<Arc<Page>, SystemError> {
        self.upgrade()?.get_or_create_page_zero(page_index)
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
        cache.mark_page_dirty(page_index);
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
        self.upgrade()
            .ok()
            .and_then(|cache| cache.lock().get_page(page_index))
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
                        cache.account_state_transition(old_state, PageState::Dirty);
                        current.set_state(PageState::Dirty);
                    }
                }
            }

            return Ok(true);
        }
    }

    pub fn sync(&self) -> Result<(), SystemError> {
        let cache = self.upgrade()?;
        let dirty_entries: Vec<(usize, Arc<PageEntry>)> = {
            let inner = cache.inner.lock();
            inner
                .dirty_pages
                .iter()
                .filter_map(|idx| inner.pages.get(idx).cloned().map(|entry| (*idx, entry)))
                .collect()
        };

        for (page_index, entry) in dirty_entries {
            Self::writeback_entry(&cache, page_index, entry)?;
        }

        // 脏页写完后调 write_inode 回写元数据。
        if let Some(inode) = cache.inode().and_then(|w| w.upgrade()) {
            let wbc = WritebackControl::sync_all_for_sync();
            if let Err(e) = inode.write_inode(&wbc) {
                log::warn!("write_inode failed: {:?}", e);
                cache.record_writeback_error_with_superblock(e.clone());
                return Err(e);
            }
        }

        Ok(())
    }

    pub fn resize(&self, len: usize) -> Result<(), SystemError> {
        let cache = self.upgrade()?;
        cache.truncate(len)
    }

    pub fn writeback_range(&self, start_index: usize, end_index: usize) -> Result<(), SystemError> {
        let cache = self.upgrade()?;
        let dirty_entries: Vec<(usize, Arc<PageEntry>)> = {
            let inner = cache.inner.lock();
            inner
                .dirty_pages
                .range(start_index..=end_index)
                .filter_map(|idx| inner.pages.get(idx).cloned().map(|entry| (*idx, entry)))
                .collect()
        };

        for (page_index, entry) in dirty_entries {
            Self::writeback_entry(&cache, page_index, entry)?;
        }

        Ok(())
    }

    pub fn wait_writeback_range(
        &self,
        start_index: usize,
        end_index: usize,
    ) -> Result<(), SystemError> {
        let cache = self.upgrade()?;
        let entries: Vec<Arc<PageEntry>> = {
            let inner = cache.inner.lock();
            inner
                .page_indices
                .range(start_index..=end_index)
                .filter_map(|idx| inner.pages.get(idx).cloned())
                .collect()
        };

        for entry in entries {
            Self::wait_writeback_entry(entry)?;
        }

        Ok(())
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
                    Self::wait_writeback_entry(entry)?;
                    continue;
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
                    cache.account_state_transition(old_state, PageState::Dirty);
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
    ) -> Result<(), SystemError> {
        let cache = self.upgrade()?;
        let dirty_entries: Vec<(usize, Arc<PageEntry>)> = {
            let inner = cache.inner.lock();
            inner
                .dirty_pages
                .range(start_index..=end_index)
                .filter_map(|idx| inner.pages.get(idx).cloned().map(|entry| (*idx, entry)))
                .collect()
        };

        for (page_index, entry) in dirty_entries {
            Self::start_writeback_entry(&cache, page_index, entry)?;
        }

        Ok(())
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
        let cache = self.upgrade()?;
        if cache.is_shmem() {
            return Ok(0);
        }
        let indices = cache.clean_evict_indices(Some((start_index, end_index)));

        let mut discarded = 0;
        for page_index in indices {
            if let Some(page) = cache.remove_clean_page_candidate(page_index) {
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
        if let Some(page) = cache.remove_clean_page_candidate(page_index) {
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
        Ok(self.upgrade()?.lock().remove_page(page_index))
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
        let cache = self.upgrade()?;
        let entry = match cache.inner.lock().get_entry(page_index) {
            Some(entry) => entry,
            None => return Ok(()),
        };
        Self::writeback_entry(&cache, page_index, entry)
    }

    fn wait_writeback_entry(entry: Arc<PageEntry>) -> Result<(), SystemError> {
        entry.wait_queue.wait_until(|| match entry.state() {
            PageState::Writeback => None,
            PageState::Error => Some(Err(SystemError::EIO)),
            _ => Some(Ok(())),
        })
    }

    fn prepare_writeback_entry(
        cache: &Arc<PageCache>,
        page_index: usize,
        entry: &Arc<PageEntry>,
    ) -> Result<bool, SystemError> {
        loop {
            match entry.state() {
                PageState::Loading => {
                    let _ = entry.wait_ready()?;
                    continue;
                }
                PageState::Writeback => {
                    Self::wait_writeback_entry(entry.clone())?;
                    continue;
                }
                PageState::Error => return Err(SystemError::EIO),
                PageState::UpToDate => {
                    let guard = entry.page.read();
                    if !guard.flags().contains(PageFlags::PG_DIRTY) {
                        return Ok(false);
                    }
                    drop(guard);
                    entry.set_state(PageState::Dirty);
                    let mut inner = cache.inner.lock();
                    inner.dirty_pages.insert(page_index);
                    continue;
                }
                PageState::Dirty => {
                    let guard = entry.page.read();
                    if !guard.flags().contains(PageFlags::PG_DIRTY) {
                        return Ok(false);
                    }
                }
            }
            if entry
                .compare_exchange_state(PageState::Dirty, PageState::Writeback)
                .is_ok()
            {
                cache.account_state_transition(PageState::Dirty, PageState::Writeback);
                let mut inner = cache.inner.lock();
                inner.dirty_pages.remove(&page_index);
                return Ok(true);
            }
        }
    }

    fn try_prepare_async_writeback_entry(
        cache: &Arc<PageCache>,
        page_index: usize,
        entry: &Arc<PageEntry>,
    ) -> Result<bool, SystemError> {
        loop {
            match entry.state() {
                PageState::Loading => {
                    let _ = entry.wait_ready()?;
                    continue;
                }
                PageState::Writeback => return Ok(false),
                PageState::Error => return Err(SystemError::EIO),
                PageState::UpToDate => {
                    let guard = entry.page.read();
                    if !guard.flags().contains(PageFlags::PG_DIRTY) {
                        return Ok(false);
                    }
                    drop(guard);
                    entry.set_state(PageState::Dirty);
                    let mut inner = cache.inner.lock();
                    inner.dirty_pages.insert(page_index);
                    continue;
                }
                PageState::Dirty => {
                    let guard = entry.page.read();
                    if !guard.flags().contains(PageFlags::PG_DIRTY) {
                        return Ok(false);
                    }
                }
            }

            if entry
                .compare_exchange_state(PageState::Dirty, PageState::Writeback)
                .is_ok()
            {
                cache.account_state_transition(PageState::Dirty, PageState::Writeback);
                let mut inner = cache.inner.lock();
                inner.dirty_pages.remove(&page_index);
                return Ok(true);
            }
        }
    }

    fn finish_writeback_entry(
        cache: Arc<PageCache>,
        page_index: usize,
        entry: Arc<PageEntry>,
        page: Arc<Page>,
        result: Result<(), SystemError>,
    ) -> Result<(), SystemError> {
        if let Err(e) = result {
            cache.record_writeback_error_with_superblock(e.clone());
            {
                let mut guard = page.write();
                guard.add_flags(PageFlags::PG_ERROR | PageFlags::PG_DIRTY);
            }
            cache.account_state_transition(PageState::Writeback, PageState::Dirty);
            entry.set_state(PageState::Dirty);
            let mut inner = cache.inner.lock();
            inner.dirty_pages.insert(page_index);
            entry.wait_queue.wake_all();
            return Err(e);
        }

        {
            let mut guard = page.write();
            guard.remove_flags(PageFlags::PG_ERROR);
        }

        let page_dirty = page.read().flags().contains(PageFlags::PG_DIRTY);
        if page_dirty {
            cache.account_state_transition(PageState::Writeback, PageState::Dirty);
            entry.set_state(PageState::Dirty);
            let mut inner = cache.inner.lock();
            inner.dirty_pages.insert(page_index);
        } else {
            cache.account_state_transition(PageState::Writeback, PageState::UpToDate);
            entry.set_state(PageState::UpToDate);
            let mut inner = cache.inner.lock();
            inner.dirty_pages.remove(&page_index);
        }
        entry.wait_queue.wake_all();
        Ok(())
    }

    fn start_writeback_entry(
        cache: &Arc<PageCache>,
        page_index: usize,
        entry: Arc<PageEntry>,
    ) -> Result<(), SystemError> {
        let _invalidate = cache.invalidate_read();
        if !Self::try_prepare_async_writeback_entry(cache, page_index, &entry)? {
            return Ok(());
        }

        let page = entry.page.clone();

        // If the inode has been freed, restore page state via finish_writeback_entry and return error.
        let inode = match cache.inode().and_then(|inode| inode.upgrade()) {
            Some(inode) => inode,
            None => {
                Self::finish_writeback_entry(
                    cache.clone(),
                    page_index,
                    entry,
                    page,
                    Err(SystemError::EIO),
                )?;
                return Err(SystemError::EIO);
            }
        };
        let backend = cache.backend();
        let page_start = page_index * MMArch::PAGE_SIZE;
        let len = if let Ok(metadata) = inode.metadata() {
            let file_size = metadata.size.max(0) as usize;
            if file_size <= page_start {
                0
            } else {
                core::cmp::min(MMArch::PAGE_SIZE, file_size - page_start)
            }
        } else {
            MMArch::PAGE_SIZE
        };

        let data = if len > 0 {
            let _ = cache.mkclean_page(page_index, false);
            let mut guard = page.write();
            guard.remove_flags(PageFlags::PG_DIRTY);
            let src = unsafe { guard.as_slice() };
            Some(src[..len].to_vec())
        } else {
            None
        };

        let cache = cache.clone();
        let work_page = page.clone();
        let work_entry = entry.clone();
        let work = Work::new(move || {
            let result = match &data {
                Some(data) => {
                    if let Some(backend) = &backend {
                        match backend.write_page(page_index, data) {
                            Ok(written) if written == data.len() => Ok(()),
                            Ok(_) => Err(SystemError::EIO),
                            Err(e) => Err(e),
                        }
                    } else {
                        inode
                            .write_direct(
                                page_start,
                                data.len(),
                                data,
                                Mutex::new(FilePrivateData::Unused).lock(),
                            )
                            .map(|_| ())
                    }
                }
                None => Ok(()),
            };
            let _ = Self::finish_writeback_entry(
                cache.clone(),
                page_index,
                work_entry.clone(),
                work_page.clone(),
                result,
            );
        });
        schedule_pagecache_io(work);
        Ok(())
    }

    fn writeback_entry(
        cache: &Arc<PageCache>,
        page_index: usize,
        entry: Arc<PageEntry>,
    ) -> Result<(), SystemError> {
        let _invalidate = cache.invalidate_read();
        if !Self::prepare_writeback_entry(cache, page_index, &entry)? {
            return Ok(());
        }

        let page = entry.page.clone();
        // RAII: if any subsequent path exits early, WritebackGuard ensures the page reverts to Dirty.
        let mut wb_guard =
            WritebackGuard::new(cache.clone(), page_index, entry.clone(), page.clone());

        let inode = cache
            .inode()
            .and_then(|inode| inode.upgrade())
            .ok_or(SystemError::EIO)?;
        let backend = cache.backend();
        let page_start = page_index * MMArch::PAGE_SIZE;
        let len = if let Ok(metadata) = inode.metadata() {
            let file_size = metadata.size.max(0) as usize;
            if file_size <= page_start {
                0
            } else {
                core::cmp::min(MMArch::PAGE_SIZE, file_size - page_start)
            }
        } else {
            MMArch::PAGE_SIZE
        };

        if len > 0 {
            let _ = cache.mkclean_page(page_index, false);
            {
                let mut guard = page.write();
                guard.remove_flags(PageFlags::PG_DIRTY);
            }
            let result = if let Some(backend) = backend {
                let data = {
                    let guard = page.read();
                    let src = unsafe { guard.as_slice() };
                    src[..len].to_vec()
                };
                match backend.write_page(page_index, &data) {
                    Ok(written) if written == data.len() => Ok(len),
                    Ok(_) => Err(SystemError::EIO),
                    Err(e) => Err(e),
                }
            } else {
                let data = unsafe {
                    core::slice::from_raw_parts(
                        MMArch::phys_2_virt(page.phys_address()).unwrap().data() as *const u8,
                        len,
                    )
                };
                inode.write_direct(
                    page_start,
                    len,
                    data,
                    Mutex::new(FilePrivateData::Unused).lock(),
                )
            };
            wb_guard.disarm();
            Self::finish_writeback_entry(cache.clone(), page_index, entry, page, result.map(|_| ()))
        } else {
            wb_guard.disarm();
            Self::finish_writeback_entry(cache.clone(), page_index, entry, page, Ok(()))
        }
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
            accounted_unevictable: AtomicBool::new(false),
            active_users: AtomicUsize::new(0),
            wait_queue: WaitQueue::default(),
        }
    }

    fn state(&self) -> PageState {
        Self::decode_state(self.state.load(Ordering::Acquire))
    }

    fn set_state(&self, state: PageState) {
        self.state.store(state as u8, Ordering::Release);
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

    fn compare_exchange_state(
        &self,
        current: PageState,
        new: PageState,
    ) -> Result<PageState, PageState> {
        self.state
            .compare_exchange(
                current as u8,
                new as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .map(Self::decode_state)
            .map_err(Self::decode_state)
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
    pub fn new(page_cache_ref: Weak<PageCache>, id: usize) -> InnerPageCache {
        Self {
            id,
            pages: HashMap::new(),
            page_indices: BTreeSet::new(),
            dirty_pages: BTreeSet::new(),
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
        if let Some(cache) = self.page_cache_ref.upgrade() {
            cache.account_entry_remove(&entry);
        }
        Some(entry.page.clone())
    }

    fn get_entry(&self, offset: usize) -> Option<Arc<PageEntry>> {
        self.pages.get(&offset).cloned()
    }

    fn insert_entry(&mut self, offset: usize, entry: Arc<PageEntry>) {
        if let Some(cache) = self.page_cache_ref.upgrade() {
            cache.account_entry_insert(&entry);
        }
        if let Some(old_entry) = self.pages.insert(offset, entry) {
            if let Some(cache) = self.page_cache_ref.upgrade() {
                cache.account_entry_remove(&old_entry);
            }
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
            if let Some(cache) = self.page_cache_ref.upgrade() {
                cache.account_entry_remove(entry);
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
        let id = PAGE_CACHE_ID.fetch_add(1, Ordering::SeqCst);
        let cache = Arc::new_cyclic(|weak| Self {
            id,
            inner: Mutex::new(InnerPageCache::new(weak.clone(), id)),
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
            is_shmem: AtomicBool::new(false),
            reclassify_lock: Mutex::new(()),
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

    /// Record a writeback error in both the page cache mapping and its
    /// mounted superblock, matching Linux mapping_set_error() semantics.
    pub fn record_writeback_error_with_superblock(&self, error: SystemError) {
        self.record_writeback_error(error.clone());
        if let Some(inode) = self.inode().and_then(|w| w.upgrade()) {
            record_writeback_error_for_fs(&inode.fs(), error);
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

    fn truncate_locked(&self, new_size: usize) -> Result<bool, SystemError> {
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

                        let page_guard = current.page.read();
                        if page_guard.map_count() != 0 {
                            retry_after_unmap = true;
                            None
                        } else {
                            drop(page_guard);
                            guard.remove_page(page_index)
                        }
                    }
                };

                if retry_after_unmap {
                    return Ok(false);
                }

                if let Some(page) = removed_page {
                    self.discard_unlinked_page(&page);
                }
                break;
            }
        }

        if new_size > 0 && !new_size.is_multiple_of(MMArch::PAGE_SIZE) {
            let last_page_index = (new_size - 1) >> MMArch::PAGE_SHIFT;
            let last_len = new_size - (last_page_index << MMArch::PAGE_SHIFT);
            let entry = {
                let guard = self.inner.lock();
                guard.get_entry(last_page_index)
            };
            if let Some(entry) = entry {
                match entry.state() {
                    PageState::Loading => {
                        let _ = entry.wait_ready();
                    }
                    PageState::Writeback => {
                        let _ = entry.wait_queue.wait_until(|| match entry.state() {
                            PageState::Writeback => None,
                            PageState::Error => Some(Err(SystemError::EIO)),
                            _ => Some(Ok(())),
                        });
                    }
                    _ => {}
                }
                unsafe {
                    entry.page.write().truncate(last_len);
                }
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
            Some((start, end)) => guard.page_indices.range(start..=end).copied().collect(),
            None => guard.page_indices.iter().copied().collect(),
        }
    }

    fn remove_clean_page_candidate(&self, page_index: usize) -> Option<Arc<Page>> {
        loop {
            let entry = {
                let guard = self.inner.lock();
                guard.get_entry(page_index)
            }?;

            match entry.state() {
                PageState::Loading => {
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
            if let Some(page) = self.remove_clean_page_candidate(page_index) {
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

    pub fn set_shmem(&self, shmem: bool) {
        self.is_shmem.store(shmem, Ordering::Relaxed);
    }

    fn is_shmem(&self) -> bool {
        self.is_shmem.load(Ordering::Relaxed)
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

                    let mut page_guard = page.write();
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
                    let guard = self.inner.lock();
                    let Some(current) = guard.pages.get(&index) else {
                        continue;
                    };
                    if !Arc::ptr_eq(current, &entry) || self.mapping_unevictable() {
                        continue;
                    }

                    let mut page_guard = page.write();
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

    fn account_entry_insert(&self, entry: &PageEntry) {
        pc_stats::inc_file_pages();
        if self.is_shmem() {
            pc_stats::inc_shmem_pages();
        }
        if self.mapping_unevictable() {
            entry.account_unevictable_if_needed();
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

    fn account_entry_remove(&self, entry: &PageEntry) {
        pc_stats::dec_file_pages();
        if self.is_shmem() {
            pc_stats::dec_shmem_pages();
        }
        entry.unaccount_unevictable_if_needed();
        let state = entry.state();
        match state {
            PageState::Dirty => pc_stats::dec_file_dirty(),
            PageState::Writeback => pc_stats::dec_file_writeback(),
            _ => {}
        }
    }
    fn account_state_transition(&self, old: PageState, new: PageState) {
        if old == new {
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

    pub fn mark_page_dirty(&self, page_index: usize) {
        let mut guard = self.inner.lock();
        if let Some(entry) = guard.get_entry(page_index) {
            let old_state = entry.state();
            guard.dirty_pages.insert(page_index);
            if old_state == PageState::Writeback {
                return;
            }
            self.account_state_transition(old_state, PageState::Dirty);
            entry.set_state(PageState::Dirty);
        }
    }

    pub fn mark_page_writeback(&self, page_index: usize) {
        let mut guard = self.inner.lock();
        if let Some(entry) = guard.get_entry(page_index) {
            let old_state = entry.state();
            self.account_state_transition(old_state, PageState::Writeback);
            entry.set_state(PageState::Writeback);
            guard.dirty_pages.remove(&page_index);
        }
    }

    pub fn mark_page_uptodate(&self, page_index: usize) {
        let mut guard = self.inner.lock();
        if let Some(entry) = guard.get_entry(page_index) {
            let old_state = entry.state();
            self.account_state_transition(old_state, PageState::UpToDate);
            entry.set_state(PageState::UpToDate);
            guard.dirty_pages.remove(&page_index);
        }
    }

    pub fn mark_page_error(&self, page_index: usize, error: SystemError) {
        self.record_writeback_error_with_superblock(error);
        let mut guard = self.inner.lock();
        if let Some(entry) = guard.get_entry(page_index) {
            let old_state = entry.state();
            self.account_state_transition(old_state, PageState::Error);
            entry.set_state(PageState::Error);
            entry.wait_queue.wake_all();
            guard.dirty_pages.remove(&page_index);
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

    /// 两阶段写入：持锁收集目标页，解锁后按页写入，避免用户缺页时持有page cache锁
    pub fn write(&self, offset: usize, buf: &[u8]) -> Result<usize, SystemError> {
        let len = buf.len();
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

        let mut src_offset = 0;
        for item in copies {
            // 预触发用户缓冲区当前段，避免后续在持页锁时缺页
            let _ = volatile_read!(buf[src_offset]);
            let mut page_guard = item.entry.page.write();
            unsafe {
                page_guard.as_slice_mut()[item.page_offset..item.page_offset + item.sub_len]
                    .copy_from_slice(&buf[src_offset..src_offset + item.sub_len]);
            }
            page_guard.add_flags(PageFlags::PG_DIRTY);
            src_offset += item.sub_len;
            drop(page_guard);
            self.mark_page_dirty(item.page_index);
        }

        Ok(ret)
    }
}
