//! RAM backing must not acquire an ordinary writeback runner or lose dirty data.
use super::*;
use crate::{
    cgroup::memory::{object::ObjectAccount, MemoryAccount},
    filesystem::{
        ramfs::RamFS,
        tmpfs,
        vfs::{
            mount::{MountFlags, SuperBlockState},
            FileSystem, FileType, InodeMode,
        },
    },
};

const TIMEOUT: Duration = Duration::from_secs(2);

struct Fixture {
    inode: Arc<dyn IndexNode>,
    cache: Arc<PageCache>,
    _fs: Option<Arc<dyn FileSystem>>,
    _file: Option<Arc<tmpfs::TmpfsShmemFile>>,
}

impl Fixture {
    fn new(tmpfs: bool) -> Result<Self, MemoryAllocationError> {
        if tmpfs {
            let file = tmpfs::create_unlinked_shmem_file(
                "resident-writeback-selftest",
                MMArch::PAGE_SIZE,
            )?;
            Ok(Self {
                inode: file.inode(),
                cache: file.page_cache(),
                _fs: None,
                _file: Some(file),
            })
        } else {
            let fs = RamFS::new();
            let root = fs.root_inode();
            let inode = root.create("resident-selftest", FileType::File, InodeMode::S_IRWXU)?;
            inode.resize(MMArch::PAGE_SIZE)?;
            let cache = inode.page_cache().ok_or(SystemError::EIO)?;
            root.unlink("resident-selftest")?;
            if !matches!(root.find("resident-selftest"), Err(SystemError::ENOENT)) {
                return Err(SystemError::EIO.into());
            }
            Ok(Self {
                inode,
                cache,
                _fs: Some(fs),
                _file: None,
            })
        }
    }
}

struct Gate {
    entered: AtomicBool,
    release: AtomicBool,
    done: AtomicBool,
    wait: WaitQueue,
}
struct ReleaseGate(Arc<Gate>);
impl Drop for ReleaseGate {
    fn drop(&mut self) {
        self.0.release.store(true, Ordering::Release);
        self.0.wait.wake_all();
    }
}

fn poll(mut ready: impl FnMut() -> bool) -> bool {
    let end = crate::time::Instant::now() + TIMEOUT;
    loop {
        if ready() {
            return true;
        }
        if crate::time::Instant::now() >= end {
            return false;
        }
        crate::sched::sched_yield();
    }
}

fn accounts() -> Result<(Arc<MemoryAccount>, Arc<ObjectAccount>), SystemError> {
    // Metadata belongs to a different account; this account measures only data.
    let data = MemoryAccount::new(0, None)?;
    let metadata = MemoryAccount::new(0, None)?;
    Ok((data, ObjectAccount::new(&metadata)?))
}

fn populate(
    cache: &PageCache,
    owner: &Arc<MemoryAccount>,
    creator: &Arc<ObjectAccount>,
) -> Result<(), MemoryAllocationError> {
    let bytes = vec![0x6d; MMArch::PAGE_SIZE];
    if cache.try_write(0, &bytes, owner, creator)? != bytes.len() {
        return Err(SystemError::EIO.into());
    }
    Ok(())
}

fn resident(cache: &PageCache, owner: &MemoryAccount) -> Result<bool, SystemError> {
    let entry = cache.inner.lock().get_entry(0).ok_or(SystemError::EIO)?;
    let mut bytes = vec![0; MMArch::PAGE_SIZE];
    Ok(cache.read(0, &mut bytes)? == bytes.len()
        && bytes.iter().all(|byte| *byte == 0x6d)
        && entry.state() == PageState::Dirty
        && entry.writeback_tag() == 0
        && entry.page.read().flags().contains(PageFlags::PG_DIRTY)
        && cache.inner.lock().dirty_pages.contains(&0)
        && cache.inner.lock().writeback_pages.is_empty()
        && owner.current_pages() == 1)
}

// Remove the clean page from the global scan set before publishing Dirty.
// An already captured snapshot can still see the Arc, so retain the page write
// lock through the explicit runner admission: global scanners use try_read().
fn private_page(
    cache: &PageCache,
    owner: &Arc<MemoryAccount>,
    creator: &Arc<ObjectAccount>,
) -> Result<Arc<Page>, MemoryAllocationError> {
    let pin = cache.try_get_or_create_page_pinned(0, false, owner, creator)?;
    let page = pin.page();
    detach_pages_from_reclaimer_batched(core::iter::once(&page));
    drop(pin);
    Ok(page)
}

fn publish_and_start(cache: &PageCache, page: &Page) -> Result<bool, SystemError> {
    let bytes = vec![0x6d; MMArch::PAGE_SIZE];
    let mut guard = page.write();
    unsafe {
        guard.as_slice_mut().copy_from_slice(&bytes);
    }
    guard.add_flags(PageFlags::PG_DIRTY);
    cache.mark_page_dirty_page_locked(0, &guard)?;
    // This entry only prepares and queues work; it does not acquire the page.
    let result = cache.manager().try_start_reclaimer_writeback_range(0, 0);
    drop(guard);
    result
}

fn lifecycle(tmpfs: bool) -> Result<bool, MemoryAllocationError> {
    let fixture = Fixture::new(tmpfs)?;
    let domain_owner = fixture.inode.fs();
    let superblock = Arc::new(SuperBlockState::new(MountFlags::empty()));
    let domain = PageCacheWritebackDomain::new_shmem();
    domain.bind(&domain_owner, &superblock)?;
    let executor = domain.writeback_executor()?;
    // The real RAM backend is reused on a private domain so the gate cannot
    // occupy the global unowned executor or delay unrelated filesystem work.
    let cache = PageCache::new_filesystem_shmem(
        Arc::downgrade(&fixture.inode),
        fixture.cache.backend().ok_or(SystemError::EIO)?,
        &domain,
    )?;
    let (owner, creator) = accounts()?;
    let page = private_page(&cache, &owner, &creator)?;
    let gate = Arc::new(Gate {
        entered: AtomicBool::new(false),
        release: AtomicBool::new(false),
        done: AtomicBool::new(false),
        wait: WaitQueue::default(),
    });
    let release = ReleaseGate(gate.clone());
    let worker = gate.clone();
    writeback::schedule_pagecache_writeback(
        &executor,
        Work::new(move || {
            worker.entered.store(true, Ordering::Release);
            worker.wait.wake_all();
            // The worker has its own deadline in addition to unconditional RAII release.
            let _ = worker.wait.wait_until_timeout(
                || worker.release.load(Ordering::Acquire).then_some(()),
                Duration::from_secs(5),
            );
            worker.done.store(true, Ordering::Release);
        }),
    );
    gate.wait.wait_until_timeout(
        || gate.entered.load(Ordering::Acquire).then_some(()),
        TIMEOUT,
    )?;
    let weak_cache = Arc::downgrade(&cache);
    let weak_inode = Arc::downgrade(&fixture.inode);
    let scheduled = publish_and_start(&cache, &page)?;
    let data_ok = resident(&cache, &owner)?;
    let active = cache.reclaimer_writeback_active.load(Ordering::Acquire);
    drop(page);
    drop(cache);
    drop(fixture);
    // Ignore a transient background snapshot, but never let the private worker
    // advance before the owner oracle. A queued runner cannot retire here.
    let retired = poll(|| {
        weak_cache.upgrade().is_none()
            && weak_inode.upgrade().is_none()
            && owner.current_pages() == 0
    });
    let blocked = !gate.done.load(Ordering::Acquire);
    let passed = data_ok && !scheduled && !active && retired && blocked;
    if !passed {
        log::error!("resident backing selftest: tmpfs={} scheduled={} active={} cache_retained={} inode_retained={} pages={} gate_blocked={}",
            tmpfs, scheduled, active, weak_cache.upgrade().is_some(), weak_inode.upgrade().is_some(), owner.current_pages(), blocked);
    }
    drop(release);
    let cleaned = poll(|| {
        gate.done.load(Ordering::Acquire)
            && weak_cache.upgrade().is_none()
            && weak_inode.upgrade().is_none()
            && owner.current_pages() == 0
    });
    Ok(passed && cleaned)
}

fn ram_sync(tmpfs: bool) -> Result<bool, MemoryAllocationError> {
    let fixture = Fixture::new(tmpfs)?;
    let cache = &fixture.cache;
    let manager = cache.manager();
    let (owner, creator) = accounts()?;
    populate(cache, &owner, &creator)?;
    manager.sync_range(0, 0)?;
    let plain = resident(cache, &owner)?;
    populate(cache, &owner, &creator)?;
    manager.sync_range_with_stable_size(0, 0, MMArch::PAGE_SIZE)?;
    let stable = resident(cache, &owner)?;
    populate(cache, &owner, &creator)?;
    manager.launder_range_for_invalidate_with_stable_size(0, 0, MMArch::PAGE_SIZE)?;
    let launder = resident(cache, &owner)?;
    populate(cache, &owner, &creator)?;
    let called = core::cell::Cell::new(false);
    let (value, range) = manager.start_writeback_range_with_freeze(0, 0, || {
        called.set(true);
        Ok(71)
    })?;
    range.wait_for_completion()?;
    let frozen = called.get()
        && value == 71
        && range.frozen_end.is_none()
        && range.epoch == 0
        && resident(cache, &owner)?;
    let failed = core::cell::Cell::new(false);
    let error = manager.start_writeback_range_with_freeze(0, 0, || {
        failed.set(true);
        Err::<(), _>(SystemError::EACCES)
    });
    let propagated =
        matches!(error, Err(SystemError::EACCES)) && failed.get() && resident(cache, &owner)?;
    let domain_idle = !<PageCache as PageCacheDomainMember>::has_dirty_or_writeback_work(cache);
    let page = cache
        .inner
        .lock()
        .get_entry(0)
        .ok_or(SystemError::EIO)?
        .page
        .clone();
    crate::mm::page::PageReclaimer::page_writeback(&mut page.write(), false);
    let legacy = resident(cache, &owner)?;
    drop(page);
    drop(fixture);
    Ok(plain
        && stable
        && launder
        && frozen
        && propagated
        && domain_idle
        && legacy
        && poll(|| owner.current_pages() == 0))
}

#[derive(Debug)]
struct OrdinaryBackend {
    writes: AtomicUsize,
    correct: AtomicBool,
}
impl PageCacheBackend for OrdinaryBackend {
    fn npages(&self) -> usize {
        1
    }
    fn read_page(&self, _: usize, _: &mut [u8]) -> Result<usize, SystemError> {
        Ok(0)
    }
    fn write_page(&self, index: usize, bytes: &[u8]) -> Result<usize, SystemError> {
        self.correct.store(
            index == 0
                && bytes.len() == MMArch::PAGE_SIZE
                && bytes.iter().all(|byte| *byte == 0x6d),
            Ordering::Release,
        );
        self.writes.fetch_add(1, Ordering::Release);
        Ok(bytes.len())
    }
}

fn ordinary() -> Result<bool, MemoryAllocationError> {
    let fixture = Fixture::new(false)?;
    let domain_owner = fixture.inode.fs();
    let superblock = Arc::new(SuperBlockState::new(MountFlags::empty()));
    let domain = PageCacheWritebackDomain::new_shmem();
    domain.bind(&domain_owner, &superblock)?;
    let _executor = domain.writeback_executor()?;
    let backend = Arc::new(OrdinaryBackend {
        writes: AtomicUsize::new(0),
        correct: AtomicBool::new(false),
    });
    // Deliberately Shmem kind: support follows the backend rather than a kind filter.
    let cache =
        PageCache::new_filesystem_shmem(Arc::downgrade(&fixture.inode), backend.clone(), &domain)?;
    let (owner, creator) = accounts()?;
    let page = private_page(&cache, &owner, &creator)?;
    let scheduled = publish_and_start(&cache, &page)?;
    drop(page);
    let completed = poll(|| {
        !cache.reclaimer_writeback_active.load(Ordering::Acquire)
            && !cache.has_dirty_or_writeback_pages()
            && backend.writes.load(Ordering::Acquire) == 1
    });
    let correct = backend.correct.load(Ordering::Acquire);
    drop(cache);
    drop(fixture);
    Ok(scheduled && completed && correct && poll(|| owner.current_pages() == 0))
}

pub(super) fn run() -> Result<bool, MemoryAllocationError> {
    // Always clean up the denied-runner cases before reporting failure. Keep the
    // first baseline evidence local instead of running a sleep-based matrix.
    let ramfs = lifecycle(false)?;
    let tmpfs = lifecycle(true)?;
    if !ramfs || !tmpfs {
        return Ok(false);
    }
    Ok(ram_sync(false)? && ram_sync(true)? && ordinary()?)
}
