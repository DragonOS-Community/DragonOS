//! Exercise production read publication while the global cleanup worker is busy.
use super::*;

const TIMEOUT: Duration = Duration::from_secs(2);

struct GlobalWorkerGate {
    started: AtomicBool,
    release: AtomicBool,
    finished: AtomicBool,
    wait: WaitQueue,
}

struct ReleaseGlobalWorker(Arc<GlobalWorkerGate>);

impl Drop for ReleaseGlobalWorker {
    fn drop(&mut self) {
        self.0.release.store(true, Ordering::Release);
        self.0.wait.wake_all();
    }
}

#[derive(Debug)]
struct ShortReadBackend {
    calls: [AtomicUsize; 2],
}

impl ShortReadBackend {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            calls: [AtomicUsize::new(0), AtomicUsize::new(0)],
        })
    }
}

impl PageCacheBackend for ShortReadBackend {
    fn read_page(&self, index: usize, buffer: &mut [u8]) -> Result<usize, SystemError> {
        self.calls[index].fetch_add(1, Ordering::AcqRel);
        // Poison the tail too: publication, not zero-initialized allocation,
        // must sanitize bytes beyond the reported short read.
        buffer.fill(0xa5);
        if index == 0 {
            buffer[..17].fill(0x5a);
            Ok(17)
        } else {
            Err(SystemError::EIO)
        }
    }

    fn write_page(&self, _: usize, buffer: &[u8]) -> Result<usize, SystemError> {
        Ok(buffer.len())
    }

    fn npages(&self) -> usize {
        2
    }
}

fn ready_before_release(entry: &Arc<PageEntry>) -> bool {
    let deadline = crate::time::Instant::now() + TIMEOUT;
    while entry.state() == PageState::Loading {
        if crate::time::Instant::now() >= deadline {
            return false;
        }
        crate::sched::sched_yield();
    }
    true
}

fn short_read_valid(entry: &Arc<PageEntry>) -> bool {
    if entry.state() != PageState::UpToDate {
        return false;
    }
    let page = entry.page.read();
    let bytes = unsafe { page.as_slice() };
    page.flags().contains(PageFlags::PG_UPTODATE)
        && bytes[..17].iter().all(|byte| *byte == 0x5a)
        && bytes[17..].iter().all(|byte| *byte == 0)
}

fn one_shot_terminal(complete_first: bool) -> Result<bool, SystemError> {
    let waiter = PageIoWaiter::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let valid_result = Arc::new(AtomicBool::new(false));
    if complete_first {
        waiter.complete(Ok(17));
        waiter.complete(Err(SystemError::EIO));
    }
    let callback_calls = calls.clone();
    let callback_valid = valid_result.clone();
    waiter.register_terminal(Box::new(move |result| {
        callback_calls.fetch_add(1, Ordering::AcqRel);
        callback_valid.store(matches!(result, Ok(17)), Ordering::Release);
    }))?;
    if !complete_first {
        waiter.complete(Ok(17));
        waiter.complete(Err(SystemError::EIO));
    }
    let duplicate_rejected = matches!(
        waiter.register_terminal(Box::new(|_| {})),
        Err(SystemError::EEXIST)
    );
    Ok(duplicate_rejected
        && calls.load(Ordering::Acquire) == 1
        && valid_result.load(Ordering::Acquire))
}

pub(super) fn run() -> Result<bool, SystemError> {
    if !one_shot_terminal(false)? || !one_shot_terminal(true)? {
        return Ok(false);
    }
    let gate = Arc::new(GlobalWorkerGate {
        started: AtomicBool::new(false),
        release: AtomicBool::new(false),
        finished: AtomicBool::new(false),
        wait: WaitQueue::default(),
    });
    let release = ReleaseGlobalWorker(gate.clone());
    let worker_gate = gate.clone();
    schedule_work(Work::new(move || {
        worker_gate.started.store(true, Ordering::Release);
        worker_gate.wait.wake_all();
        worker_gate
            .wait
            .wait_until(|| worker_gate.release.load(Ordering::Acquire).then_some(()));
        worker_gate.finished.store(true, Ordering::Release);
        worker_gate.wait.wake_all();
    }));
    if gate
        .wait
        .wait_until_timeout(|| gate.started.load(Ordering::Acquire).then_some(()), TIMEOUT)
        .is_err()
    {
        return Ok(false);
    }

    let single_backend = ShortReadBackend::new();
    let single = PageCache::new_unowned(None, Some(single_backend.clone()));
    let batch_backend = ShortReadBackend::new();
    let batch = PageCache::new_unowned(None, Some(batch_backend.clone()));
    let result = (|| {
        single.start_async_read(0)?;
        single.start_async_read(1)?;
        let single_good = single.inner.lock().get_entry(0).ok_or(SystemError::EIO)?;
        let single_error = single.inner.lock().get_entry(1).ok_or(SystemError::EIO)?;
        let completion = PageCacheReadBatchCompletion::reserve(&batch, 0, 2, None)?;
        // Pin identities before submit; error completion legitimately removes
        // a failed batch slot from the mapping.
        let batch_good = batch.inner.lock().get_entry(0).ok_or(SystemError::EIO)?;
        let batch_error = batch.inner.lock().get_entry(1).ok_or(SystemError::EIO)?;
        batch_backend.clone().submit_read_batch(
            PageCacheReadBatchRequest {
                start_index: 0,
                page_count: 2,
                valid_bytes: 2 * MMArch::PAGE_SIZE,
            },
            completion,
        );
        let ready = [&single_good, &single_error, &batch_good, &batch_error]
            .into_iter()
            .all(ready_before_release);
        Ok(ready
            && short_read_valid(&single_good)
            && short_read_valid(&batch_good)
            && single_error.state() == PageState::Error
            && batch_error.state() == PageState::Error
            && single_error.page.read().flags().contains(PageFlags::PG_ERROR)
            && batch_error.page.read().flags().contains(PageFlags::PG_ERROR)
            && single_backend.calls.iter().all(|call| call.load(Ordering::Acquire) == 1)
            && batch_backend.calls.iter().all(|call| call.load(Ordering::Acquire) == 1)
            && !gate.release.load(Ordering::Acquire))
    })();
    // Release even on failure before dropping caches: old kernels can still
    // have Loading work queued behind this gate. Never reproduce by hanging
    // the selftest process or leaving SYSTEM_WQ permanently occupied.
    drop(release);
    let released = gate
        .wait
        .wait_until_timeout(|| gate.finished.load(Ordering::Acquire).then_some(()), TIMEOUT)
        .is_ok();
    drop(single);
    drop(batch);
    result.map(|passed| passed && released)
}
