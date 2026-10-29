use alloc::{
    boxed::Box,
    string::{String, ToString},
    sync::{Arc, Weak},
};
use core::{
    fmt,
    sync::atomic::{AtomicBool, Ordering},
};
use lazy_static::lazy_static;
use log;
use system_error::SystemError;

use crate::{
    libs::{resource_wait, spinlock::SpinLock},
    process::{
        kthread::{KernelThreadClosure, KernelThreadMechanism},
        ProcessControlBlock,
    },
};

#[cfg(feature = "mm_selftest")]
#[path = "workqueue/selftest.rs"]
mod selftest;

#[cfg(feature = "mm_selftest")]
pub(crate) fn heap_denied_workqueue_selftest() -> Result<bool, SystemError> {
    selftest::run()
}

/// Represents a work item to be executed in a workqueue.
pub struct Work {
    func: Box<dyn Fn() + Send + Sync>,
    next: SpinLock<Option<Arc<Work>>>,
    pending: AtomicBool,
}

impl Work {
    /// Create a new work item from a closure or function.
    pub fn new<F>(f: F) -> Arc<Self>
    where
        F: Fn() + Send + Sync + 'static,
    {
        Arc::new(Self {
            func: Box::new(f),
            next: SpinLock::new(None),
            pending: AtomicBool::new(false),
        })
    }

    /// Prepare a work item before acquiring resources needed by its callback.
    pub fn try_new<F>(f: F) -> Result<Arc<Self>, SystemError>
    where
        F: Fn() + Send + Sync + 'static,
    {
        let func: Box<dyn Fn() + Send + Sync> = Box::try_new(f).map_err(|_| SystemError::ENOMEM)?;
        Arc::try_new(Self {
            func,
            next: SpinLock::new(None),
            pending: AtomicBool::new(false),
        })
        .map_err(|_| SystemError::ENOMEM)
    }

    /// Execute the work item.
    pub fn run(&self) {
        (self.func)();
    }
}

impl fmt::Debug for Work {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Work")
            .field("pending", &self.pending.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Default)]
struct WorkList {
    head: Option<Arc<Work>>,
    tail: Option<Weak<Work>>,
}

/// A workqueue that manages a list of works and a worker thread.
pub struct WorkQueue {
    queue: SpinLock<WorkList>,
    wake_token: Arc<AtomicBool>,
    worker: SpinLock<Option<Arc<ProcessControlBlock>>>,
}

impl fmt::Debug for WorkQueue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WorkQueue").finish_non_exhaustive()
    }
}

impl WorkQueue {
    /// Create a new workqueue with the given name.
    /// This will spawn a kernel thread with the given name.
    pub fn new(name: &str) -> Arc<Self> {
        let wq = Arc::new(Self {
            queue: SpinLock::new(WorkList::default()),
            wake_token: Arc::new(AtomicBool::new(false)),
            worker: SpinLock::new(None),
        });

        // Create worker thread
        let wq_clone = wq.clone();
        let closure = Box::new(move || worker_loop(wq_clone.clone()));
        let kt_closure = KernelThreadClosure::EmptyClosure((closure, ()));

        let worker_pcb = KernelThreadMechanism::create_and_run(kt_closure, name.to_string())
            .expect("Failed to create workqueue worker");

        *wq.worker.lock() = Some(worker_pcb);

        wq
    }

    /// Create a serial queue whose worker exits when its last owner is dropped.
    ///
    /// Callers must retain the queue until their work finishes; a work item may
    /// retain its owner for this purpose. Unlike `new`, the sleeping worker
    /// does not keep the queue or its owner alive indefinitely.
    pub fn try_new_owned(name: &str) -> Result<Arc<Self>, SystemError> {
        let wake_token = Arc::try_new(AtomicBool::new(false)).map_err(|_| SystemError::ENOMEM)?;
        let wq = Arc::try_new(Self {
            queue: SpinLock::new(WorkList::default()),
            wake_token,
            worker: SpinLock::new(None),
        })
        .map_err(|_| SystemError::ENOMEM)?;
        let queue = Arc::downgrade(&wq);
        let wake_token = wq.wake_token.clone();
        let closure = Box::try_new(move || owned_worker_loop(queue.clone(), wake_token.clone()))
            .map_err(|_| SystemError::ENOMEM)?;
        let mut worker_name = String::new();
        worker_name
            .try_reserve_exact(name.len())
            .map_err(|_| SystemError::ENOMEM)?;
        worker_name.push_str(name);
        KernelThreadMechanism::create_and_run(
            KernelThreadClosure::EmptyClosure((closure, ())),
            worker_name,
        )
        .ok_or(SystemError::ENOMEM)?;
        Ok(wq)
    }

    /// Enqueue a work item to the workqueue.
    pub fn enqueue(&self, work: Arc<Work>) {
        let mut queue = self.queue.lock();
        if work.pending.swap(true, Ordering::AcqRel) {
            return;
        }

        debug_assert!(work.next.lock().is_none());
        let new_tail = Arc::downgrade(&work);
        if let Some(tail) = queue.tail.take().and_then(|tail| tail.upgrade()) {
            *tail.next.lock() = Some(work);
        } else {
            debug_assert!(queue.head.is_none());
            queue.head = Some(work);
        }
        queue.tail = Some(new_tail);
        drop(queue);
        self.notify_worker();
    }

    fn pop(&self) -> Option<Arc<Work>> {
        let mut queue = self.queue.lock();
        let work = queue.head.take()?;
        queue.head = work.next.lock().take();
        if queue.head.is_none() {
            queue.tail = None;
        }
        // Clear PENDING while holding the queue lock. The work may enqueue
        // itself once run() starts without racing this dequeue operation.
        work.pending.store(false, Ordering::Release);
        Some(work)
    }

    fn notify_worker(&self) {
        self.wake_token.store(true, Ordering::Release);
        resource_wait::notify_resource_all(Arc::as_ptr(&self.wake_token) as usize);
    }
}

impl Drop for WorkQueue {
    fn drop(&mut self) {
        // An owned worker waits without holding a strong queue reference.
        self.notify_worker();
    }
}

fn wait_for_work(wake_token: &AtomicBool, interruptible: bool) -> Result<(), SystemError> {
    // Only the scalar token is examined under the wait protocol. In particular,
    // a temporary last queue owner must not be destroyed in this predicate.
    resource_wait::wait_for_resource(
        wake_token as *const AtomicBool as usize,
        interruptible,
        || wake_token.swap(false, Ordering::AcqRel).then_some(()),
    )
}

fn owned_worker_loop(queue: Weak<WorkQueue>, wake_token: Arc<AtomicBool>) -> i32 {
    loop {
        let Some(owner) = queue.upgrade() else {
            return 0;
        };
        let work = owner.pop();
        // Weak ownership lets dropping the last external queue owner wake and
        // terminate an idle worker. All owner retirement stays outside wait.
        drop(owner);
        if let Some(work) = work {
            work.run();
            continue;
        }
        wait_for_work(&wake_token, false).expect("owned workqueue wait failed");
    }
}

/// The main loop for the permanently owned worker thread.
fn worker_loop(wq: Arc<WorkQueue>) -> i32 {
    loop {
        while let Some(work) = wq.pop() {
            work.run();
        }
        // Preserve the ordinary worker's interruptible wait. A notification
        // remembered between the empty check and registration avoids lost work.
        let _ = wait_for_work(&wq.wake_token, true);
    }
}

lazy_static! {
    /// The system-wide default workqueue.
    pub static ref SYSTEM_WQ: Arc<WorkQueue> = WorkQueue::new("events");
}

/// Schedule a work item to the system default workqueue.
pub fn schedule_work(work: Arc<Work>) {
    SYSTEM_WQ.enqueue(work);
}

/// Initialize the workqueue subsystem.
pub fn workqueue_init() {
    // Trigger lazy_static initialization
    lazy_static::initialize(&SYSTEM_WQ);
    test_workqueue();
}

fn test_workqueue() {
    let work = Work::new(|| {
        log::info!("Workqueue test: Hello from worker thread!");
    });
    schedule_work(work);
}
