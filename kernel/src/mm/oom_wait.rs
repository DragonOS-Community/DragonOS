//! Prepared metadata for OOM recovery waits; not the recovery policy itself.
//!
//! The queue does not allocate while registering, cancelling or broadcasting.
//! Waking still invokes the scheduler and is NOT allocation-free until the
//! scheduler's separate preparation work is complete. Broadcast only from a
//! resource-lock-free task context; do not call from a charge/token destructor.

use alloc::{boxed::Box, sync::Arc};
use core::sync::atomic::{fence, AtomicUsize, Ordering};
use system_error::SystemError;

use crate::libs::{
    spinlock::SpinLock,
    wait_queue::{Waiter, Waker},
};

struct Node {
    waker: Arc<Waker>,
    next: Option<Box<Node>>,
}

impl Drop for Node {
    fn drop(&mut self) {
        // A queue can have a long chain. Never recursively destroy it.
        let mut next = self.next.take();
        while let Some(mut node) = next {
            next = node.next.take();
            drop(node);
        }
    }
}

pub(crate) struct PreparedOomWait {
    waiter: Waiter,
    node: Box<Node>,
}

impl PreparedOomWait {
    /// Call before selection/kill, with no MM/resource/IRQ locks held.
    pub(crate) fn new() -> Result<Self, SystemError> {
        let (waiter, waker) = Waiter::try_new_pair()?;
        let node = Box::try_new(Node { waker, next: None }).map_err(|_| SystemError::ENOMEM)?;
        Ok(Self { waiter, node })
    }

    /// Publish once, then check the caller's real recovery predicate before
    /// sleeping. Spurious wakes reuse this registration and its original node.
    pub(crate) fn register(self, queue: &OomWaitQueue) -> OomWaitRegistration<'_> {
        let Self { waiter, mut node } = self;
        let mut head = queue.head.lock_irqsave();
        node.next = head.take();
        *head = Some(node);
        queue.registered.fetch_add(1, Ordering::Release);
        drop(head);
        // Pair with the producer's full barrier before its lockless empty
        // check. After returning, the caller must recheck its real predicate.
        fence(Ordering::SeqCst);
        OomWaitRegistration { queue, waiter }
    }
}

pub(crate) struct OomWaitRegistration<'q> {
    queue: &'q OomWaitQueue,
    waiter: Waiter,
}

impl OomWaitRegistration<'_> {
    /// The caller owns scope/victim/epoch validation; waking is not recovery.
    pub(crate) fn wait_killable(&self) -> Result<(), SystemError> {
        self.waiter.wait_killable()
    }

    /// Control writes stop on any pending signal, unlike allocation's
    /// killable OOM wait. Both reuse the same prepared registration.
    pub(crate) fn wait_interruptible(&self) -> Result<(), SystemError> {
        self.waiter.wait(true)
    }
}

impl Drop for OomWaitRegistration<'_> {
    fn drop(&mut self) {
        let waker = self.waiter.waker();
        // A broadcast may temporarily own our node. Closing before unlink
        // prevents it from restoring a registration after this handle exits.
        waker.close();
        self.queue.unregister(&waker);
        self.queue.registered.fetch_sub(1, Ordering::AcqRel);
    }
}

pub(crate) struct OomWaitQueue {
    head: SpinLock<Option<Box<Node>>>,
    // Counts registration handles, including nodes temporarily held by a
    // broadcaster. A detached head must never masquerade as no waiters.
    registered: AtomicUsize,
}

impl OomWaitQueue {
    pub(crate) const fn new() -> Self {
        Self {
            head: SpinLock::new(None),
            registered: AtomicUsize::new(0),
        }
    }

    pub(crate) fn has_registrations(&self) -> bool {
        self.registered.load(Ordering::Acquire) != 0
    }

    fn unregister(&self, waker: &Arc<Waker>) {
        let mut head = self.head.lock_irqsave();
        let mut cursor = &mut *head;
        let retired = loop {
            match cursor {
                Some(node) if Arc::ptr_eq(&node.waker, waker) => {
                    let mut node = cursor.take().unwrap();
                    *cursor = node.next.take();
                    break Some(node);
                }
                Some(node) => cursor = &mut node.next,
                None => break None, // The broadcaster owns this node in flight.
            }
        };
        drop(head);
        drop(retired);
    }

    /// Restore each node before waking, so a waiter consuming the notification
    /// can immediately wait again without allocating or missing registration.
    /// Last-reference destruction and scheduler wake happen outside the lock.
    pub(crate) fn wake_all(&self) -> usize {
        let mut pending = self.head.lock_irqsave().take();
        let mut woken = 0;
        while let Some(mut node) = pending {
            pending = node.next.take();
            let waker = node.waker.clone();
            let restored = {
                let mut head = self.head.lock_irqsave();
                if waker.is_closed() {
                    false
                } else {
                    node.next = head.take();
                    *head = Some(node);
                    true
                }
            };
            // An inactive node was never moved into the queue. Its Box and
            // final Waker reference are retired only after releasing the guard.
            if restored {
                woken += usize::from(waker.wake());
            }
        }
        woken
    }

    #[cfg(test)]
    pub(crate) fn registrations(&self) -> usize {
        let head = self.head.lock_irqsave();
        let mut node = head.as_deref();
        let mut count = 0;
        while let Some(current) = node {
            count += 1;
            node = current.next.as_deref();
        }
        count
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detached_long_chain_is_retired_iteratively() {
        let (_waiter, waker) = Waiter::try_new_pair().unwrap();
        let mut head = None;
        for _ in 0..30_000 {
            head = Some(
                Box::try_new(Node {
                    waker: waker.clone(),
                    next: head,
                })
                .unwrap(),
            );
        }
        drop(head);
        assert_eq!(Arc::strong_count(&waker), 2);
    }
}
