//! Retire displaced lazy-MM owners in the existing memory worker, never on
//! a scheduler stack. One embedded node coalesces every queued owner of an MM.

use alloc::sync::Arc;

use crate::{libs::spinlock::SpinLock, mm::ucontext::AddressSpace};

#[cfg(feature = "mm_selftest")]
pub(crate) mod selftest;

#[cfg(feature = "mm_selftest")]
pub(crate) fn selftest_report_check() -> Result<bool, system_error::SystemError> {
    selftest::run()
}

/// Access only under QUEUE, including when a node belongs to a detached batch.
#[derive(Debug, Default)]
pub(crate) struct LazyMmDropLink {
    next: Option<Arc<AddressSpace>>,
    queued: bool,
}

struct Queue {
    head: Option<Arc<AddressSpace>>,
}

impl Queue {
    const fn new() -> Self {
        Self { head: None }
    }

    /// Caller holds global QUEUE even for a private test/batch queue.
    fn push(&mut self, mm: Arc<AddressSpace>) {
        let mut link = mm.lazy_drop_link.lock();
        if link.queued {
            drop(link);
            // The live queue or detached batch still pins this exact MM.
            drop(mm);
            return;
        }
        debug_assert!(link.next.is_none());
        link.next = self.head.take();
        link.queued = true;
        drop(link);
        self.head = Some(mm);
    }

    fn take_all(&mut self) -> Self {
        Self {
            head: self.head.take(),
        }
    }

    /// Detach next before returning the owner, so its Drop cannot recurse down
    /// the batch chain. Caller holds global QUEUE through clearing queued.
    fn pop(&mut self) -> Option<Arc<AddressSpace>> {
        let mm = self.head.take()?;
        let mut link = mm.lazy_drop_link.lock();
        self.head = link.next.take();
        debug_assert!(link.queued);
        link.queued = false;
        drop(link);
        Some(mm)
    }
}

static QUEUE: SpinLock<Queue> = SpinLock::new(Queue::new());

/// The caller has replaced this CPU's loaded MM and cleared old active CPU
/// membership. Consume its last possibly terminal temporary before switching
/// stacks; neither coalescing nor publication allocates or retires a last owner.
pub(crate) fn enqueue(mm: Arc<AddressSpace>) {
    QUEUE.lock_irqsave().push(mm);
    super::memory_notify::request_lazy_mm_drop();
}

/// Only the resource-lock-free memory worker may retire these owners. A finite
/// detached batch bounds each round; concurrent enqueues become later work.
pub(crate) fn drain() {
    let mut batch = QUEUE.lock_irqsave().take_all();
    loop {
        let mm = {
            let _queue = QUEUE.lock_irqsave();
            batch.pop()
        };
        let Some(mm) = mm else { break };
        drop(mm);
        crate::sched::cond_resched();
    }
}
