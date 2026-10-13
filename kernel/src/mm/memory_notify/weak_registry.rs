//! Prepared, weak registrations for task-context memory notifications.
//!
//! Only registration allocates. A broadcaster temporarily owns the chain,
//! invokes callbacks without the registry lock, and restores each live node
//! before releasing its temporary strong owner. Thus an owner's final Drop
//! can always unlink its restored node, without a registry ownership cycle.

use alloc::{
    boxed::Box,
    sync::{Arc, Weak},
};
use core::sync::atomic::{AtomicUsize, Ordering};
use system_error::SystemError;

use crate::libs::spinlock::SpinLock;

struct Node<T> {
    id: usize,
    owner: Weak<T>,
    next: Option<Box<Self>>,
}

impl<T> Drop for Node<T> {
    fn drop(&mut self) {
        let mut next = self.next.take();
        while let Some(mut node) = next {
            next = node.next.take();
            drop(node);
        }
    }
}

pub(crate) struct WeakRegistry<T> {
    head: SpinLock<Option<Box<Node<T>>>>,
    registered: AtomicUsize,
}

impl<T> WeakRegistry<T> {
    pub(crate) const fn new() -> Self {
        Self {
            head: SpinLock::new(None),
            registered: AtomicUsize::new(0),
        }
    }

    /// Keep the returned identity in the owner; unregister in its final Drop.
    /// The owner must remain strongly held until this method returns.
    pub(crate) fn register(
        &self,
        owner: &Arc<T>,
        initialize: impl FnOnce(usize),
    ) -> Result<(), SystemError> {
        let id = Arc::as_ptr(owner) as usize;
        let mut node = Box::try_new(Node {
            id,
            owner: Arc::downgrade(owner),
            next: None,
        })
        .map_err(|_| SystemError::ENOMEM)?;
        let mut head = self.head.lock_irqsave();
        // Publish the owner's identity before a broadcaster can see its node.
        // initialize must only update already prepared owner metadata.
        initialize(id);
        node.next = head.take();
        *head = Some(node);
        self.registered.fetch_add(1, Ordering::Release);
        Ok(())
    }

    pub(crate) fn has_registrations(&self) -> bool {
        self.registered.load(Ordering::Acquire) != 0
    }

    pub(crate) fn unregister(&self, id: usize) {
        if id == 0 {
            return;
        }
        let removed = {
            let mut head = self.head.lock_irqsave();
            let mut link = &mut *head;
            loop {
                match link.as_ref() {
                    Some(node) if node.id == id => {
                        let mut node = link.take().unwrap();
                        *link = node.next.take();
                        break Some(node);
                    }
                    Some(_) => link = &mut link.as_mut().unwrap().next,
                    None => break None,
                }
            }
        };
        // A dead owner may have been encountered while the broadcaster owns
        // its node. Its weak node will then be retired by that broadcaster.
        self.registered.fetch_sub(1, Ordering::AcqRel);
        drop(removed);
    }

    /// Must run in resource-lock-free task context, never an IRQ/destructor.
    pub(crate) fn for_each(&self, mut callback: impl FnMut(&T), is_active: impl Fn(&T) -> bool) {
        let mut batch = self.head.lock_irqsave().take();
        while let Some(mut node) = batch {
            batch = node.next.take();
            if let Some(owner) = node.owner.upgrade() {
                callback(&owner);
                let retired = {
                    let mut head = self.head.lock_irqsave();
                    // Logical close can precede the last temporary Arc. Its
                    // atomic mark is checked while holding the same lock as
                    // unlink, so close cannot resurrect a detached node.
                    if is_active(&owner) {
                        node.next = head.take();
                        *head = Some(node);
                        None
                    } else {
                        Some(node)
                    }
                };
                drop(retired);
                // Last-owner destruction now sees the restored registration.
                drop(owner);
            } else {
                drop(node);
            }
        }
    }
}
