//! Deterministic stacked Legacy writeback on the shared unowned executor.
use super::*;

const TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug)]
struct RawTestInode;

impl IndexNode for RawTestInode {
    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }
    fn fs(&self) -> Arc<dyn crate::filesystem::vfs::FileSystem> {
        crate::filesystem::ramfs::RamFS::new()
    }
    fn read_at(
        &self,
        _offset: usize,
        _len: usize,
        _bytes: &mut [u8],
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, MemoryAllocationError> {
        Err(SystemError::ENOSYS.into())
    }

    fn write_at(
        &self,
        _offset: usize,
        _len: usize,
        _bytes: &[u8],
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, MemoryAllocationError> {
        Err(SystemError::ENOSYS.into())
    }

    fn metadata(&self) -> Result<crate::filesystem::vfs::Metadata, SystemError> {
        let mut metadata = crate::filesystem::vfs::Metadata::new(
            crate::filesystem::vfs::FileType::File,
            crate::filesystem::vfs::InodeMode::from_bits_truncate(0o600),
        );
        metadata.size = MMArch::PAGE_SIZE as i64;
        Ok(metadata)
    }
}

#[derive(Debug)]
struct RawTestState {
    lower: Mutex<Weak<PageCache>>,
    abort: AtomicBool,
    nested_ok: AtomicBool,
    record_seen: AtomicBool,
    writes: AtomicUsize,
}

impl RawTestState {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            lower: Mutex::new(Weak::new()),
            abort: AtomicBool::new(false),
            nested_ok: AtomicBool::new(false),
            record_seen: AtomicBool::new(false),
            writes: AtomicUsize::new(0),
        })
    }
}

#[derive(Debug)]
struct RawTestBackend {
    state: Arc<RawTestState>,
    cache: Mutex<Weak<PageCache>>,
    outer: bool,
}

impl PageCacheBackend for RawTestBackend {
    fn read_page(&self, _: usize, bytes: &mut [u8]) -> Result<usize, SystemError> {
        bytes.fill(0);
        Ok(bytes.len())
    }
    fn write_page(&self, _: usize, bytes: &[u8]) -> Result<usize, SystemError> {
        if self.outer {
            if self.state.abort.load(Ordering::Acquire) {
                return Err(SystemError::EIO);
            }
            let lower = self.state.lower.lock().upgrade().ok_or(SystemError::EIO)?;
            // A broken dispatch policy must fail instead of parking the sole
            // worker on a lower payload queued behind this very callback.
            if lower.has_writeback_pages() {
                return Err(SystemError::EDEADLK_OR_EDEADLOCK);
            }
            lower
                .manager()
                .launder_range_for_invalidate_with_stable_size(0, 0, MMArch::PAGE_SIZE)?;
            lower.manager().wait_writeback_range(0, 0)?;
            self.state
                .nested_ok
                .store(!lower.has_dirty_or_writeback_pages(), Ordering::Release);
        } else {
            let cache = self.cache.lock().upgrade().ok_or(SystemError::EIO)?;
            let pending = cache.tagged_writeback_submissions.lock();
            self.state
                .record_seen
                .store(pending.len() == 1, Ordering::Release);
        }
        self.state.writes.fetch_add(1, Ordering::Release);
        Ok(bytes.len())
    }
    fn npages(&self) -> usize {
        1
    }
    fn writeback_claim_in_worker(&self) -> bool {
        true
    }
}

struct ExecutorGate {
    entered: AtomicBool,
    release: AtomicBool,
    wait: WaitQueue,
}
struct ReleaseExecutor(Arc<ExecutorGate>);
impl Drop for ReleaseExecutor {
    fn drop(&mut self) {
        self.0.release.store(true, Ordering::Release);
        self.0.wait.wake_all();
    }
}

fn idle(cache: &PageCache) -> bool {
    !cache.has_dirty_or_writeback_pages()
        && cache.tagged_writeback_submissions.lock().is_empty()
        && cache
            .inner
            .lock()
            .pages
            .values()
            .all(|entry| entry.writeback_tag() == 0)
}

pub(super) fn run() -> Result<bool, SystemError> {
    let state = RawTestState::new();
    let lower_inode: Arc<dyn IndexNode> = Arc::new(RawTestInode);
    let outer_inode: Arc<dyn IndexNode> = Arc::new(RawTestInode);
    let lower_backend = Arc::new(RawTestBackend {
        state: state.clone(),
        cache: Mutex::new(Weak::new()),
        outer: false,
    });
    let outer_backend = Arc::new(RawTestBackend {
        state: state.clone(),
        cache: Mutex::new(Weak::new()),
        outer: true,
    });
    let lower = PageCache::new_unowned(
        Some(Arc::downgrade(&lower_inode)),
        Some(lower_backend.clone()),
    );
    let outer = PageCache::new_unowned(
        Some(Arc::downgrade(&outer_inode)),
        Some(outer_backend.clone()),
    );
    *lower_backend.cache.lock() = Arc::downgrade(&lower);
    *outer_backend.cache.lock() = Arc::downgrade(&outer);
    *state.lower.lock() = Arc::downgrade(&lower);
    let bytes = vec![0x5a; MMArch::PAGE_SIZE];
    lower.write(0, &bytes)?;
    outer.write(0, &bytes)?;

    let executor = outer.writeback_executor()?;
    let gate = Arc::new(ExecutorGate {
        entered: AtomicBool::new(false),
        release: AtomicBool::new(false),
        wait: WaitQueue::default(),
    });
    let release = ReleaseExecutor(gate.clone());
    let worker_gate = gate.clone();
    writeback::schedule_pagecache_writeback(
        &executor,
        Work::new(move || {
            worker_gate.entered.store(true, Ordering::Release);
            worker_gate.wait.wake_all();
            // Every failure path releases the gate, including an unwinding caller.
            // The worker also has its own deadline, so no assertion strands it.
            let _ = worker_gate.wait.wait_until_timeout(
                || worker_gate.release.load(Ordering::Acquire).then_some(()),
                TIMEOUT,
            );
        }),
    );
    gate.wait.wait_until_timeout(
        || gate.entered.load(Ordering::Acquire).then_some(()),
        TIMEOUT,
    )?;
    let outer_range = outer.manager().start_writeback_range(0, 0)?;
    let lower_range = lower.manager().start_writeback_range(0, 0)?;
    let deferred = [&outer, &lower].iter().all(|cache| {
        let inner = cache.inner.lock();
        inner
            .get_entry(0)
            .is_some_and(|entry| entry.state() == PageState::Dirty && entry.writeback_tag() != 0)
            && inner.writeback_pages.is_empty()
    });
    state.abort.store(!deferred, Ordering::Release);
    drop(release);

    let deadline = crate::time::Instant::now() + TIMEOUT;
    while !(idle(&outer) && idle(&lower)) {
        if crate::time::Instant::now() >= deadline {
            // If tagged laundering regresses to ignoring tags, release its
            // dependency by retiring an unsubmitted lower page. This is real
            // mapping invalidation, never fabricated writeback completion.
            if !lower.has_writeback_pages() {
                let _ = lower.manager().resize(0);
            }
            let cleanup_deadline = crate::time::Instant::now() + TIMEOUT;
            while outer.has_writeback_pages() && crate::time::Instant::now() < cleanup_deadline {
                crate::sched::sched_yield();
            }
            if !outer.has_writeback_pages() {
                let _ = outer.manager().resize(0);
            }
            return Ok(false);
        }
        crate::sched::sched_yield();
    }
    // The idle predicate proves these production waits cannot park on this
    // test's worker: both Dirty tags and pending submission records are gone.
    outer_range.wait_for_submission()?;
    outer_range.wait_for_completion()?;
    lower_range.wait_for_submission()?;
    lower_range.wait_for_completion()?;
    let ok = deferred
        && state.nested_ok.load(Ordering::Acquire)
        && state.record_seen.load(Ordering::Acquire)
        && state.writes.load(Ordering::Acquire) == 2;
    lower.manager().resize(0)?;
    outer.manager().resize(0)?;
    Ok(ok)
}
