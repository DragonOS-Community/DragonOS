//! Mount-owned batch scheduling and bounded completion ownership.
//!
//! The disk worker never runs inode cleanup. Cleanup has a separate thread:
//! it may need io/pool locks held by a caller waiting for another disk batch.
//! Both workers use weak owners while asleep and are joined at final unmount.

use super::filesystem::Ext4FileSystem;
use crate::{
    libs::{mutex::Mutex, wait_queue::WaitQueue},
    process::{
        kthread::{KernelThreadClosure, KernelThreadMechanism},
        ProcessControlBlock, ProcessManager,
    },
    time::{Duration, Instant},
};
use alloc::{
    boxed::Box,
    string::ToString,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::{
    fmt,
    sync::atomic::{AtomicU64, Ordering},
};
use system_error::SystemError;

const COMPLETION_SLOTS: usize = 128;
const COMMIT_DEADLINE_US: u64 = 100_000;

/// Gate progress wakes only foreground metadata retries. In particular a
/// read-view release must not schedule journal or completion work.
pub(super) struct Ext4MetadataMutationWait {
    pub(super) wait_queue: WaitQueue,
}
impl Ext4MetadataMutationWait {
    pub(super) fn new() -> Self {
        Self {
            wait_queue: WaitQueue::default(),
        }
    }
}
impl kdepends::another_ext4::MetadataMutationWaker for Ext4MetadataMutationWait {
    fn wake_all(&self) {
        self.wait_queue.wake_all();
    }
}

/// Batch progress additionally drives the disk worker. Capacity/checkpoint
/// changes also unblock foreground retries whose generation includes batch state.
struct JournalWake {
    wait_queue: WaitQueue,
    event: AtomicU64,
    first_notification: AtomicU64,
    metadata: Arc<Ext4MetadataMutationWait>,
}
impl JournalWake {
    fn notify(&self) {
        if self.first_notification.load(Ordering::Relaxed) == 0 {
            let _ = self.first_notification.compare_exchange(
                0,
                now_us(),
                Ordering::AcqRel,
                Ordering::Relaxed,
            );
        }
        self.event.fetch_add(1, Ordering::Release);
        self.wait_queue.wake_all();
    }
}
impl kdepends::another_ext4::MetadataMutationWaker for JournalWake {
    fn wake_all(&self) {
        self.metadata.wait_queue.wake_all();
        self.notify();
    }
}

fn now_us() -> u64 {
    Instant::now().total_micros().max(1) as u64
}

/// A pre-existing boxed submission can implement this directly. In particular,
/// registration after Published must not allocate a boxed closure or queue node.
pub(super) trait Ext4JournalCompletion: Send {
    fn complete(self: Box<Self>, result: Result<(), SystemError>);
}

enum CompletionSlot {
    Free,
    Reserved,
    Pending {
        sequence: u64,
        context: Box<dyn Ext4JournalCompletion>,
    },
    Completing,
}

struct JournalState {
    slots: Vec<CompletionSlot>,
    in_flight: usize,
    durable: u64,
    error: Option<SystemError>,
    error_reported: bool,
    closed: bool,
    stopping: bool,
    sync_requests: usize,
}

pub(super) struct Ext4Journal {
    filesystem: Mutex<Weak<Ext4FileSystem>>,
    state: Mutex<JournalState>,
    wake: Arc<JournalWake>,
    completion_wait: Arc<WaitQueue>,
    disk_worker: Mutex<Option<Arc<ProcessControlBlock>>>,
    completion_worker: Mutex<Option<Arc<ProcessControlBlock>>>,
}

impl fmt::Debug for Ext4Journal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = self.state.lock();
        f.debug_struct("Ext4Journal")
            .field("in_flight", &state.in_flight)
            .field("durable", &state.durable)
            .field("closed", &state.closed)
            .finish()
    }
}

pub(super) struct Ext4CompletionReservation {
    journal: Arc<Ext4Journal>,
    slot: Option<usize>,
}

impl Ext4CompletionReservation {
    /// Infallible ownership transfer after logical publication, including
    /// publication racing failure or an already-advanced durable frontier.
    pub(super) fn publish(mut self, sequence: u64, context: Box<dyn Ext4JournalCompletion>) {
        let index = self
            .slot
            .take()
            .expect("completion reservation consumed once");
        let force = {
            let mut state = self.journal.state.lock();
            assert!(matches!(state.slots[index], CompletionSlot::Reserved));
            state.slots[index] = CompletionSlot::Pending { sequence, context };
            state.sync_requests != 0
        };
        if force {
            self.journal.request_commit();
        }
        self.journal.completion_wait.wake_all();
    }
}

impl Drop for Ext4CompletionReservation {
    fn drop(&mut self) {
        if let Some(index) = self.slot.take() {
            self.journal.release_slot(index);
        }
    }
}

pub(super) struct Ext4JournalSyncRequest {
    journal: Arc<Ext4Journal>,
}

impl crate::filesystem::vfs::FileSystemSyncGuard for Ext4JournalSyncRequest {}

impl Drop for Ext4JournalSyncRequest {
    fn drop(&mut self) {
        let mut state = self.journal.state.lock();
        debug_assert!(state.sync_requests != 0);
        state.sync_requests -= 1;
    }
}

impl Ext4Journal {
    pub(super) fn new(wake: Arc<Ext4MetadataMutationWait>) -> Result<Arc<Self>, SystemError> {
        let mut slots = Vec::new();
        slots
            .try_reserve_exact(COMPLETION_SLOTS)
            .map_err(|_| SystemError::ENOMEM)?;
        slots.resize_with(COMPLETION_SLOTS, || CompletionSlot::Free);
        Ok(Arc::new(Self {
            filesystem: Mutex::new(Weak::new()),
            state: Mutex::new(JournalState {
                slots,
                in_flight: 0,
                durable: 0,
                error: None,
                error_reported: false,
                closed: false,
                stopping: false,
                sync_requests: 0,
            }),
            wake: Arc::new(JournalWake {
                wait_queue: WaitQueue::default(),
                event: AtomicU64::new(0),
                first_notification: AtomicU64::new(0),
                metadata: wake,
            }),
            completion_wait: Arc::new(WaitQueue::default()),
            disk_worker: Mutex::new(None),
            completion_worker: Mutex::new(None),
        }))
    }

    pub(super) fn progress_waker(&self) -> Arc<dyn kdepends::another_ext4::MetadataMutationWaker> {
        self.wake.clone()
    }

    /// Called once, after the filesystem root is initialized but before the
    /// canonical device instance is visible to a mount caller.
    pub(super) fn start(self: &Arc<Self>, fs: &Arc<Ext4FileSystem>) -> Result<(), SystemError> {
        *self.filesystem.lock() = Arc::downgrade(fs);
        let weak = Arc::downgrade(self);
        let wake = self.wake.clone();
        let disk = KernelThreadMechanism::create_and_run(
            KernelThreadClosure::EmptyClosure((
                Box::new(move || disk_loop(weak.clone(), wake.clone())),
                (),
            )),
            "ext4_journal".to_string(),
        )
        .ok_or(SystemError::ENOMEM)?;
        *self.disk_worker.lock() = Some(disk);
        let weak = Arc::downgrade(self);
        let wake = self.completion_wait.clone();
        let completion = KernelThreadMechanism::create_and_run(
            KernelThreadClosure::EmptyClosure((
                Box::new(move || completion_loop(weak.clone(), wake.clone())),
                (),
            )),
            "ext4_complete".to_string(),
        );
        let Some(completion) = completion else {
            let _ = self.stop_and_join();
            return Err(SystemError::ENOMEM);
        };
        *self.completion_worker.lock() = Some(completion);
        self.wake.notify();
        Ok(())
    }

    fn try_reserve(self: &Arc<Self>) -> Option<Result<Ext4CompletionReservation, SystemError>> {
        let mut state = self.state.lock();
        if let Some(error) = state.error.clone() {
            return Some(Err(error));
        }
        if state.closed || state.stopping {
            return Some(Err(SystemError::EROFS));
        }
        let index = state
            .slots
            .iter()
            .position(|slot| matches!(slot, CompletionSlot::Free))?;
        state.slots[index] = CompletionSlot::Reserved;
        state.in_flight += 1;
        Some(Ok(Ext4CompletionReservation {
            journal: self.clone(),
            slot: Some(index),
        }))
    }

    /// Must be called without inode, admission, pool or metadata locks. On
    /// capacity pressure both progress producers remain independently runnable,
    /// even if every bounded PageCache worker is waiting for a reservation.
    pub(super) fn reserve_completion(
        self: &Arc<Self>,
    ) -> Result<Ext4CompletionReservation, SystemError> {
        if let Some(result) = self.try_reserve() {
            return result;
        }
        self.request_commit();
        self.completion_wait.wait_until_io(|| self.try_reserve())
    }

    fn release_slot(&self, index: usize) {
        {
            let mut state = self.state.lock();
            debug_assert!(matches!(
                state.slots[index],
                CompletionSlot::Reserved | CompletionSlot::Completing
            ));
            state.slots[index] = CompletionSlot::Free;
            debug_assert!(state.in_flight != 0);
            state.in_flight -= 1;
        }
        self.completion_wait.wake_all();
    }

    pub(super) fn sync_request(self: &Arc<Self>) -> Ext4JournalSyncRequest {
        {
            let mut state = self.state.lock();
            state.sync_requests = state
                .sync_requests
                .checked_add(1)
                .expect("sync request count overflow");
        }
        self.request_commit();
        Ext4JournalSyncRequest {
            journal: self.clone(),
        }
    }

    fn request_commit(&self) {
        let filesystem = self.filesystem.lock().upgrade();
        if let Some(fs) = filesystem {
            fs.fs.request_batch_commit();
        }
        self.wake.notify();
    }

    pub(super) fn wait_through(&self, sequence: u64) -> Result<(), SystemError> {
        self.request_commit();
        self.completion_wait.wait_until_io(|| {
            let state = self.state.lock();
            if let Some(error) = state.error.clone() {
                Some(Err(error))
            } else if state.durable >= sequence {
                Some(Ok(()))
            } else if state.stopping {
                Some(Err(SystemError::EIO))
            } else {
                None
            }
        })
    }

    pub(super) fn shutdown(&self, fs: &Ext4FileSystem) -> Result<(), SystemError> {
        self.state.lock().closed = true;
        self.completion_wait.wake_all();
        fs.fs.close_batch_admission();
        let target = fs
            .fs
            .batch_progress()
            .map_or(0, |progress| progress.accepted);
        let result = self.wait_through(target);
        self.completion_wait.wait_until_io(|| {
            let state = self.state.lock();
            // Even with no data-completion owners, a pure metadata failure
            // must reach superblock errseq before the completion worker exits.
            (state.in_flight == 0 && (state.error.is_none() || state.error_reported)).then_some(())
        });
        result.and(self.stop_and_join())
    }

    /// Also used for a factory/attachment failure before normal VFS teardown.
    /// If the last temporary fs owner is dropped by a worker itself, request
    /// its exit without joining itself; its loop returns immediately afterwards.
    pub(super) fn stop_and_join(&self) -> Result<(), SystemError> {
        self.state.lock().stopping = true;
        self.wake.notify();
        self.completion_wait.wake_all();
        let workers = [
            self.disk_worker.lock().take(),
            self.completion_worker.lock().take(),
        ];
        let mut error = None;
        for worker in workers.into_iter().flatten() {
            let result = if worker.raw_pid() == ProcessManager::current_pid() {
                KernelThreadMechanism::request_stop(&worker).map(|_| 0)
            } else {
                KernelThreadMechanism::stop(&worker)
            };
            if let Err(failure) = result {
                error.get_or_insert(failure);
            }
        }
        error.map_or(Ok(()), Err)
    }

    fn observe(&self, durable: u64, failed: bool) {
        let changed = {
            let mut state = self.state.lock();
            let changed = state.durable != durable || (failed && state.error.is_none());
            state.durable = durable;
            if failed {
                state.error.get_or_insert(SystemError::EIO);
            }
            changed
        };
        if changed {
            self.completion_wait.wake_all();
        }
    }
}

fn disk_loop(owner: Weak<Ext4Journal>, wake: Arc<JournalWake>) -> i32 {
    let mut oldest = None::<u64>;
    loop {
        let observed = wake.event.load(Ordering::Acquire);
        let notified = wake.first_notification.swap(0, Ordering::AcqRel);
        let timeout = {
            let Some(journal) = owner.upgrade() else {
                return 0;
            };
            let state = journal.state.lock();
            if state.stopping {
                return 0;
            }
            let force = state.sync_requests != 0;
            drop(state);
            let Some(fs) = journal.filesystem.lock().upgrade() else {
                return 0;
            };
            let Some(progress) = fs.fs.batch_progress() else {
                return 0;
            };
            journal.observe(progress.durable, progress.failed);
            if progress.failed || progress.running_operations == 0 {
                oldest = None;
            } else {
                let began = oldest.get_or_insert(if notified == 0 { now_us() } else { notified });
                if !progress.seal_requested
                    && (force || now_us().saturating_sub(*began) >= COMMIT_DEADLINE_US)
                {
                    fs.fs.request_batch_commit();
                }
            }
            let progress = fs
                .fs
                .batch_progress()
                .expect("batch mode cannot change after mount");
            if progress.seal_requested
                && !progress.active_operation
                && !progress.publishing
                && !progress.failed
            {
                // The next Running set may be populated during this I/O.
                // Freeze notifications seed its deadline conservatively early.
                oldest = None;
                let result = fs.fs.commit_pending_batch();
                let after = fs.fs.batch_progress().expect("mounted batch mode");
                journal.observe(after.durable, result.is_err() || after.failed);
            }
            if progress.seal_requested {
                // An active operation must release its token to make this
                // seal runnable. Its generation wake is the producer edge;
                // an expired deadline must not become a one-microsecond poll.
                None
            } else {
                oldest.map(|start| {
                    Duration::from_micros(
                        COMMIT_DEADLINE_US
                            .saturating_sub(now_us().saturating_sub(start))
                            .max(1),
                    )
                })
            }
        };
        // No filesystem/journal strong owner is retained while sleeping.
        let ready = || {
            if wake.event.load(Ordering::Acquire) != observed || owner.strong_count() == 0 {
                Some(())
            } else {
                None
            }
        };
        if let Some(timeout) = timeout {
            let _ = wake.wait_queue.wait_until_timeout(ready, timeout);
        } else {
            wake.wait_queue.wait_until(ready);
        }
    }
}

fn completion_loop(owner: Weak<Ext4Journal>, wake: Arc<WaitQueue>) -> i32 {
    loop {
        // Predicate and waiter registration share the normal WaitQueue protocol;
        // no event generation or polling is needed for completion ownership.
        let action = wake.wait_until(|| {
            let Some(journal) = owner.upgrade() else {
                return Some(None);
            };
            let mut state = journal.state.lock();
            if state.stopping {
                return Some(None);
            }
            let report = state.error.is_some() && !state.error_reported;
            let index = state.slots.iter().position(|slot| match slot {
                CompletionSlot::Pending { sequence, .. } => {
                    state.error.is_some() || *sequence <= state.durable
                }
                _ => false,
            });
            if !report && index.is_none() {
                return None;
            }
            let ready = index.map(|index| {
                let CompletionSlot::Pending { sequence, context } =
                    core::mem::replace(&mut state.slots[index], CompletionSlot::Completing)
                else {
                    unreachable!()
                };
                let result = if sequence <= state.durable {
                    Ok(())
                } else {
                    Err(state
                        .error
                        .clone()
                        .expect("non-durable completion requires failure"))
                };
                (index, context, result)
            });
            drop(state);
            Some(Some((journal, report, ready)))
        });
        let Some((journal, report_error, ready)) = action else {
            return 0;
        };
        if report_error {
            let filesystem = journal.filesystem.lock().upgrade();
            if let Some(fs) = filesystem {
                fs.writeback_domain.record_writeback_error(SystemError::EIO);
                fs.fail_stop_lifecycle();
            }
            // Publish reporting completion only after errseq/lifecycle update.
            journal.state.lock().error_reported = true;
            journal.completion_wait.wake_all();
        }
        if let Some((index, context, result)) = ready {
            // No journal/metadata lock remains held across inode cleanup.
            context.complete(result);
            journal.release_slot(index);
        }
    }
}
