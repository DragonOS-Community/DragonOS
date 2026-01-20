use core::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};

use alloc::{
    collections::BTreeSet,
    sync::{Arc, Weak},
    vec::Vec,
};
use hashbrown::HashMap;
use system_error::SystemError;

use super::vfs::{FilePrivateData, IndexNode};
use crate::exception::workqueue::{schedule_work, Work, WorkQueue};
use crate::libs::mutex::MutexGuard;
use crate::libs::spinlock::SpinLock;
use crate::libs::wait_queue::WaitQueue;
use crate::mm::page::FileMapInfo;
use crate::sched::completion::Completion;
use crate::{arch::mm::LockedFrameAllocator, libs::lazy_init::Lazy};
use crate::{
    arch::MMArch,
    libs::mutex::Mutex,
    mm::{
        page::{page_manager_lock, page_reclaimer_lock, Page, PageFlags},
        MemoryManagementArch,
    },
};
use crate::{libs::align::page_align_up, mm::page::PageType};
use lazy_static::lazy_static;

static PAGE_CACHE_ID: AtomicUsize = AtomicUsize::new(0);

const PAGECACHE_IO_WORKERS: usize = 4;
static PAGECACHE_IO_RR: AtomicUsize = AtomicUsize::new(0);

lazy_static! {
    static ref PAGECACHE_IO_WQS: Vec<Arc<WorkQueue>> = {
        let mut wqs = Vec::new();
        for i in 0..PAGECACHE_IO_WORKERS {
            wqs.push(WorkQueue::new(&format!("pagecache-io-{i}")));
        }
        wqs
    };
}

fn schedule_pagecache_io(work: Arc<Work>) {
    let idx = PAGECACHE_IO_RR.fetch_add(1, Ordering::Relaxed) % PAGECACHE_IO_WQS.len();
    PAGECACHE_IO_WQS[idx].enqueue(work);
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
    unevictable: AtomicBool,
    manager: PageCacheManager,
}

#[derive(Debug)]
pub struct InnerPageCache {
    #[allow(unused)]
    id: usize,
    pages: HashMap<usize, Arc<PageEntry>>,
    dirty_pages: BTreeSet<usize>,
    page_cache_ref: Weak<PageCache>,
}

/// 描述一次从页缓存到目标缓冲区的拷贝
pub struct CopyItem {
    entry: Arc<PageEntry>,
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

    pub fn commit_overwrite(&self, page_index: usize) -> Result<Arc<Page>, SystemError> {
        self.upgrade()?.get_or_create_page_zero(page_index)
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

    pub fn get_page_any(&self, page_index: usize) -> Option<Arc<Page>> {
        self.upgrade()
            .ok()
            .and_then(|cache| cache.lock().get_page(page_index))
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

        Ok(())
    }

    pub fn resize(&self, len: usize) -> Result<(), SystemError> {
        self.upgrade()?.lock().resize(len)
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

    pub fn invalidate_range(
        &self,
        start_index: usize,
        end_index: usize,
    ) -> Result<usize, SystemError> {
        Ok(self
            .upgrade()?
            .lock()
            .invalidate_range(start_index, end_index))
    }

    pub fn pages_count(&self) -> Result<usize, SystemError> {
        Ok(self.upgrade()?.lock().pages_count())
    }

    pub fn remove_page(&self, page_index: usize) -> Result<Option<Arc<Page>>, SystemError> {
        Ok(self.upgrade()?.lock().remove_page(page_index))
    }

    pub fn writeback_page(&self, page_index: usize) -> Result<(), SystemError> {
        let cache = self.upgrade()?;
        let entry = match cache.inner.lock().get_entry(page_index) {
            Some(entry) => entry,
            None => return Ok(()),
        };
        Self::writeback_entry(&cache, page_index, entry)
    }

    fn writeback_entry(
        cache: &Arc<PageCache>,
        page_index: usize,
        entry: Arc<PageEntry>,
    ) -> Result<(), SystemError> {
        let page = entry.page.clone();
        loop {
            match entry.state() {
                PageState::Loading => {
                    let _ = entry.wait_ready()?;
                    continue;
                }
                PageState::Writeback => {
                    entry.wait_queue.wait_until(|| match entry.state() {
                        PageState::Writeback => None,
                        PageState::Error => Some(Err(SystemError::EIO)),
                        _ => Some(Ok(())),
                    })?;
                    continue;
                }
                PageState::Error => return Err(SystemError::EIO),
                PageState::UpToDate => {
                    let guard = page.read();
                    if !guard.flags().contains(PageFlags::PG_DIRTY) {
                        return Ok(());
                    }
                    drop(guard);
                    entry.set_state(PageState::Dirty);
                    let mut inner = cache.inner.lock();
                    inner.dirty_pages.insert(page_index);
                    continue;
                }
                PageState::Dirty => {
                    let guard = page.read();
                    if !guard.flags().contains(PageFlags::PG_DIRTY) {
                        return Ok(());
                    }
                }
            }
            if entry
                .compare_exchange_state(PageState::Dirty, PageState::Writeback)
                .is_ok()
            {
                break;
            }
        }
        {
            let mut inner = cache.inner.lock();
            inner.dirty_pages.remove(&page_index);
        }

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
            {
                let mut guard = page.write();
                guard.remove_flags(PageFlags::PG_DIRTY);
            }
            let result = if let Some(backend) = backend {
                let waiter = backend.write_page_async(page_index, &page, len);
                waiter.wait().map(|_| len)
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
            if let Err(e) = result {
                page.write().add_flags(PageFlags::PG_ERROR);
                entry.set_state(PageState::Error);
                entry.wait_queue.wake_all();
                return Err(e);
            }
        }

        {
            let mut guard = page.write();
            guard.remove_flags(PageFlags::PG_ERROR);
            if guard.flags().contains(PageFlags::PG_DIRTY) {
                entry.set_state(PageState::Dirty);
                let mut inner = cache.inner.lock();
                inner.dirty_pages.insert(page_index);
            } else {
                entry.set_state(PageState::UpToDate);
                let mut inner = cache.inner.lock();
                inner.dirty_pages.remove(&page_index);
            }
        }
        entry.wait_queue.wake_all();
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
            wait_queue: WaitQueue::default(),
        }
    }

    fn state(&self) -> PageState {
        Self::decode_state(self.state.load(Ordering::Acquire))
    }

    fn set_state(&self, state: PageState) {
        self.state.store(state as u8, Ordering::Release);
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

impl InnerPageCache {
    pub fn new(page_cache_ref: Weak<PageCache>, id: usize) -> InnerPageCache {
        Self {
            id,
            pages: HashMap::new(),
            dirty_pages: BTreeSet::new(),
            page_cache_ref,
        }
    }

    pub fn get_page(&self, offset: usize) -> Option<Arc<Page>> {
        self.pages.get(&offset).map(|entry| entry.page.clone())
    }

    pub fn remove_page(&mut self, offset: usize) -> Option<Arc<Page>> {
        self.dirty_pages.remove(&offset);
        self.pages.remove(&offset).map(|entry| entry.page.clone())
    }

    fn get_entry(&self, offset: usize) -> Option<Arc<PageEntry>> {
        self.pages.get(&offset).cloned()
    }

    fn insert_entry(&mut self, offset: usize, entry: Arc<PageEntry>) {
        self.pages.insert(offset, entry);
    }

    fn is_page_ready(&self, offset: usize) -> bool {
        self.pages
            .get(&offset)
            .map(|entry| entry.state().is_ready())
            .unwrap_or(false)
    }

    pub fn resize(&mut self, len: usize) -> Result<(), SystemError> {
        let page_num = page_align_up(len) / MMArch::PAGE_SIZE;

        let mut reclaimer = page_reclaimer_lock();
        for (i, entry) in self.pages.drain_filter(|index, entry| {
            *index >= page_num && entry.state().is_ready() && entry.state() != PageState::Writeback
        }) {
            self.dirty_pages.remove(&i);
            let _ = reclaimer.remove_page(&entry.page.phys_address());
        }

        if page_num > 0 {
            let last_page_index = page_num - 1;
            let last_len = len - last_page_index * MMArch::PAGE_SIZE;
            if let Some(page) = self.get_page(last_page_index) {
                unsafe {
                    page.write().truncate(last_len);
                };
            }
            // 对于新文件，最后一页不存在是正常的，不需要返回错误
            // 只有当文件需要截断到更小的尺寸时，才需要处理最后一页
        }

        Ok(())
    }

    pub fn pages_count(&self) -> usize {
        return self.pages.len();
    }

    /// 驱逐指定范围的干净页
    ///
    /// 只驱逐干净的、无外部引用的页
    pub fn invalidate_range(&mut self, start_index: usize, end_index: usize) -> usize {
        let mut evicted = 0;
        let mut page_reclaimer = page_reclaimer_lock();

        for idx in start_index..=end_index {
            if let Some(entry) = self.pages.get(&idx) {
                if matches!(entry.state(), PageState::Loading | PageState::Writeback) {
                    continue;
                }
                let guard = entry.page.read();
                if guard.flags().contains(PageFlags::PG_DIRTY) {
                    continue;
                }
                drop(guard);

                // 3处引用：1. page_cache中 2. page_manager中 3. lru中
                if Arc::strong_count(&entry.page) <= 3 {
                    if let Some(removed) = self.pages.remove(&idx) {
                        self.dirty_pages.remove(&idx);
                        let paddr = removed.page.phys_address();
                        page_manager_lock().remove_page(&paddr);
                        let _ = page_reclaimer.remove_page(&paddr);
                        evicted += 1;
                    }
                }
            }
        }

        evicted
    }
}

impl Drop for InnerPageCache {
    fn drop(&mut self) {
        // log::debug!("page cache drop");
        let mut page_manager = page_manager_lock();
        for entry in self.pages.values() {
            page_manager.remove_page(&entry.page.phys_address());
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
        Arc::new_cyclic(|weak| Self {
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
            unevictable: AtomicBool::new(false),
            manager: PageCacheManager::new(weak.clone()),
        })
    }

    /// # 获取页缓存的ID
    #[inline]
    #[allow(unused)]
    pub fn id(&self) -> usize {
        self.id
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

    /// Mark this page cache as unevictable (or revert). When enabled, newly created
    /// pages will carry PG_UNEVICTABLE to keep the reclaimer from reclaiming them.
    pub fn set_unevictable(&self, unevictable: bool) {
        self.unevictable.store(unevictable, Ordering::Relaxed);
    }

    fn page_flags(&self) -> PageFlags {
        if self.unevictable.load(Ordering::Relaxed) {
            PageFlags::PG_LRU | PageFlags::PG_UNEVICTABLE
        } else {
            PageFlags::PG_LRU
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

        let mut page = Some(self.allocate_page(
            page_cache_ref.expect("page_cache_ref should exist"),
            page_index,
        )?);

        let (entry, need_populate) = {
            let mut guard = self.inner.lock();
            if let Some(entry) = guard.get_entry(page_index) {
                (entry, false)
            } else {
                let entry = Arc::new(PageEntry::new(
                    page.take().expect("allocated page must exist"),
                    PageState::Loading,
                ));
                guard.insert_entry(page_index, entry.clone());
                (entry, true)
            }
        };

        if !need_populate {
            if let Some(page) = page.take() {
                self.discard_unlinked_page(&page);
            }
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

    fn remove_failed_entry(&self, page_index: usize, entry: &Arc<PageEntry>) {
        let mut guard = self.inner.lock();
        if let Some(current) = guard.get_entry(page_index) {
            if Arc::ptr_eq(&current, entry) {
                guard.pages.remove(&page_index);
            }
        }
        self.discard_unlinked_page(&entry.page);
    }

    fn discard_unlinked_page(&self, page: &Arc<Page>) {
        let paddr = page.phys_address();
        page_manager_lock().remove_page(&paddr);
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

        let page = self.allocate_page(
            page_cache_ref.expect("page_cache_ref should exist"),
            page_index,
        )?;

        let entry = {
            let mut guard = self.inner.lock();
            if guard.get_entry(page_index).is_some() {
                self.discard_unlinked_page(&page);
                return Ok(());
            }
            let entry = Arc::new(PageEntry::new(page, PageState::Loading));
            guard.insert_entry(page_index, entry.clone());
            entry
        };

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

    pub fn get_or_create_page_for_read(&self, page_index: usize) -> Result<Arc<Page>, SystemError> {
        Ok(self.get_or_create_entry(page_index, true)?.page.clone())
    }

    pub fn get_or_create_page_zero(&self, page_index: usize) -> Result<Arc<Page>, SystemError> {
        Ok(self.get_or_create_entry(page_index, false)?.page.clone())
    }

    pub fn mark_page_dirty(&self, page_index: usize) {
        let mut guard = self.inner.lock();
        if let Some(entry) = guard.get_entry(page_index) {
            guard.dirty_pages.insert(page_index);
            if entry.state() == PageState::Writeback {
                return;
            }
            entry.set_state(PageState::Dirty);
        }
    }

    pub fn mark_page_writeback(&self, page_index: usize) {
        let mut guard = self.inner.lock();
        if let Some(entry) = guard.get_entry(page_index) {
            entry.set_state(PageState::Writeback);
            guard.dirty_pages.remove(&page_index);
        }
    }

    pub fn mark_page_uptodate(&self, page_index: usize) {
        let mut guard = self.inner.lock();
        if let Some(entry) = guard.get_entry(page_index) {
            entry.set_state(PageState::UpToDate);
            guard.dirty_pages.remove(&page_index);
        }
    }

    pub fn mark_page_error(&self, page_index: usize) {
        let mut guard = self.inner.lock();
        if let Some(entry) = guard.get_entry(page_index) {
            entry.set_state(PageState::Error);
            entry.wait_queue.wake_all();
            guard.dirty_pages.remove(&page_index);
        }
    }

    /// Insert a pre-allocated page into page cache and mark it ready.
    /// This is for special in-kernel users (e.g. perf ring buffers).
    pub fn insert_ready_page(&self, page_index: usize, page: Arc<Page>) -> Result<(), SystemError> {
        let mut guard = self.inner.lock();
        if guard.get_entry(page_index).is_some() {
            return Err(SystemError::EEXIST);
        }
        let entry = Arc::new(PageEntry::new(page, PageState::UpToDate));
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

            let entry = self.get_or_create_entry(page_index, true)?;
            copies.push(CopyItem {
                entry,
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

            let entry = self.get_or_create_entry(page_index, false)?;
            copies.push(CopyItem {
                entry,
                page_index,
                page_offset: write_start - page_start,
                sub_len: page_write_len,
            });
            ret += page_write_len;
        }

        let mut src_offset = 0;
        let mut dirty_page_indices: Vec<usize> = Vec::new();
        for item in copies {
            // 预触发用户缓冲区当前段，避免后续在持页锁时缺页
            let _ = volatile_read!(buf[src_offset]);
            let mut page_guard = item.entry.page.write();
            unsafe {
                page_guard.as_slice_mut()[item.page_offset..item.page_offset + item.sub_len]
                    .copy_from_slice(&buf[src_offset..src_offset + item.sub_len]);
            }
            page_guard.add_flags(PageFlags::PG_DIRTY);
            item.entry.set_state(PageState::Dirty);
            dirty_page_indices.push(item.page_index);
            src_offset += item.sub_len;
        }

        if !dirty_page_indices.is_empty() {
            let mut guard = self.inner.lock();
            for page_index in dirty_page_indices {
                guard.dirty_pages.insert(page_index);
            }
        }

        Ok(ret)
    }
}
