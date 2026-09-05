use alloc::{
    boxed::Box,
    string::ToString,
    sync::{Arc, Weak},
};
use core::{
    fmt,
    sync::atomic::{AtomicBool, Ordering},
};
use lazy_static::lazy_static;
use log;

use crate::{
    libs::{spinlock::SpinLock, wait_queue::WaitQueue},
    process::{
        kthread::{KernelThreadClosure, KernelThreadMechanism},
        ProcessControlBlock,
    },
};

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
    wait_queue: Arc<WaitQueue>,
    worker: SpinLock<Option<Arc<ProcessControlBlock>>>,
}

impl WorkQueue {
    /// Create a new workqueue with the given name.
    /// This will spawn a kernel thread with the given name.
    pub fn new(name: &str) -> Arc<Self> {
        let wq = Arc::new(Self {
            queue: SpinLock::new(WorkList::default()),
            wait_queue: Arc::new(WaitQueue::default()),
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
        self.wait_queue.wakeup(None);
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

    fn is_empty(&self) -> bool {
        self.queue.lock().head.is_none()
    }
}

/// The main loop for the worker thread.
fn worker_loop(wq: Arc<WorkQueue>) -> i32 {
    loop {
        // Wait for work
        let _ = wq
            .wait_queue
            .wait_event_interruptible(|| !wq.is_empty(), None::<fn()>);

        // Process works
        loop {
            let work = wq.pop();
            match work {
                Some(w) => w.run(),
                None => break,
            }
        }
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
