//! Host checks of the production FIFO; stubs do not model kernel scheduling.
extern crate alloc;
extern crate self as system_error;
#[derive(Debug, PartialEq)]
pub enum SystemError {
    ESHUTDOWN,
}
mod libs {
    pub mod spinlock {
        pub struct SpinLock<T>(std::sync::Mutex<T>);
        impl<T> SpinLock<T> {
            pub fn new(value: T) -> Self {
                Self(std::sync::Mutex::new(value))
            }
            pub fn lock_irqsave(&self) -> std::sync::MutexGuard<'_, T> {
                self.0.lock().unwrap()
            }
        }
    }
    pub mod wait_queue {
        #[derive(Default)]
        pub struct WaitQueue;
        impl WaitQueue {
            pub fn wakeup(&self, _: Option<()>) {}
            pub fn wakeup_all(&self, _: Option<()>) {}
            pub fn wait_event_uninterruptible(
                &self,
                condition: impl Fn() -> bool,
                _: Option<fn()>,
            ) -> Result<(), ()> {
                assert!(condition(), "host harness must not block");
                Ok(())
            }
        }
    }
}
mod bio {
    pub struct BioRequest;
}
mod bio_queue {
    include!("../../kernel/src/driver/base/block/bio_queue.rs");
}
use bio::BioRequest;
use bio_queue::BioQueue;
use std::sync::Arc;

fn drain(queue: &BioQueue, remaining: usize) -> Vec<Arc<BioRequest>> {
    queue.drain_batch(remaining)
}

#[test]
fn uneven_batches_preserve_budget_and_ownership() {
    let queue = BioQueue::new();
    let requests: Vec<_> = (0..33).map(|_| Arc::new(BioRequest)).collect();
    queue.submit(requests[0].clone()).unwrap();
    let mut out = drain(&queue, 32);
    assert_eq!(out.len(), 1);
    for request in &requests[1..] {
        queue.submit(request.clone()).unwrap();
    }
    out.extend(drain(&queue, 31));
    assert_eq!(out.len(), 17);
    out.extend(drain(&queue, 15));
    assert_eq!(
        out.len(),
        32,
        "a worker must never dequeue beyond its budget"
    );
    let last = drain(&queue, 32);
    assert_eq!(last.len(), 1);
    for (actual, expected) in out.iter().chain(last.iter()).zip(&requests) {
        assert!(Arc::ptr_eq(actual, expected));
    }
    assert!(drain(&queue, 32).is_empty());
}

#[test]
fn zero_budget_preserves_fifo_and_stop_rejects_submission() {
    let queue = BioQueue::new();
    let first = Arc::new(BioRequest);
    let second = Arc::new(BioRequest);
    queue.submit(first.clone()).unwrap();
    queue.submit(second.clone()).unwrap();
    assert!(drain(&queue, 0).is_empty());
    assert!(matches!(
        queue.wait_for_work_or_stop(),
        bio_queue::BioQueueWake::WorkAvailable
    ));
    queue.begin_quiesce();
    assert_eq!(
        queue.submit(Arc::new(BioRequest)),
        Err(SystemError::ESHUTDOWN)
    );
    let out = drain(&queue, 1);
    assert_eq!(out.len(), 1);
    assert!(Arc::ptr_eq(&out[0], &first));
    let pending = queue.stop_and_drain();
    assert_eq!(pending.len(), 1);
    assert!(Arc::ptr_eq(&pending[0], &second));
    assert!(queue.stop_and_drain().is_empty());
    assert!(drain(&queue, 16).is_empty());
    assert_eq!(queue.submit(first), Err(SystemError::ESHUTDOWN));
    assert!(matches!(
        queue.wait_for_work_or_stop(),
        bio_queue::BioQueueWake::Stopping
    ));
}
