use core::{
    cell::UnsafeCell,
    ptr::NonNull,
    sync::atomic::{AtomicBool, Ordering},
};

use super::{gp::RcuSequence, RcuRawCallback};

struct RcuHeadNode {
    next: Option<NonNull<RcuHead>>,
    func: Option<RcuRawCallback>,
    target_gp: Option<RcuSequence>,
    callback_seq: Option<RcuSequence>,
}

impl RcuHeadNode {
    const fn new() -> Self {
        Self {
            next: None,
            func: None,
            target_gp: None,
            callback_seq: None,
        }
    }
}

/// Intrusive storage for one ordinary-RCU callback.
///
/// Once submitted, this value must remain initialized at the same address
/// until its callback starts. `queued` detects duplicate admission; it is not
/// a lifetime reference count.
pub struct RcuHead {
    queued: AtomicBool,
    node: UnsafeCell<RcuHeadNode>,
}

impl RcuHead {
    pub const fn new() -> Self {
        Self {
            queued: AtomicBool::new(false),
            node: UnsafeCell::new(RcuHeadNode::new()),
        }
    }

    /// Claims this head for one admission without modifying its queue node.
    pub(super) fn try_claim(&self) -> bool {
        self.queued
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    pub(super) fn is_queued(&self) -> bool {
        self.queued.load(Ordering::Acquire)
    }
}

impl core::fmt::Debug for RcuHead {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("RcuHead")
            .field("queued", &self.queued.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

// SAFETY: `queued` is atomic. All access to `node` is encapsulated by
// `RcuCallbackList` and serialized by `RcuState::inner`. The unsafe admission
// contract keeps a queued head initialized at a stable address.
unsafe impl Send for RcuHead {}
// SAFETY: Shared references only expose atomic duplicate admission. The
// non-atomic node remains protected by the global RCU state lock.
unsafe impl Sync for RcuHead {}

pub(super) struct ReadyRcuCallback {
    pub(super) head: NonNull<RcuHead>,
    pub(super) func: RcuRawCallback,
    pub(super) seq: RcuSequence,
}

/// The one global intrusive callback FIFO.
///
/// The containing `RcuState::inner` lock must be held for every operation.
pub(super) struct RcuCallbackList {
    head: Option<NonNull<RcuHead>>,
    tail: Option<NonNull<RcuHead>>,
    len: usize,
}

impl RcuCallbackList {
    pub(super) const fn new() -> Self {
        Self {
            head: None,
            tail: None,
            len: 0,
        }
    }

    pub(super) fn is_empty(&self) -> bool {
        self.head.is_none()
    }

    pub(super) fn len(&self) -> usize {
        self.len
    }

    pub(super) fn front_target(&self) -> Option<RcuSequence> {
        let head = self.head?;
        // SAFETY: Queue operations are serialized by `RcuState::inner`, and
        // admission keeps every linked head alive at a stable address.
        unsafe { (*head.as_ref().node.get()).target_gp }
    }

    pub(super) fn push(
        &mut self,
        head: NonNull<RcuHead>,
        func: RcuRawCallback,
        target_gp: RcuSequence,
        callback_seq: RcuSequence,
    ) {
        // SAFETY: The caller holds `RcuState::inner`, has exclusively claimed
        // `head`, and guarantees its address and lifetime through callback
        // start. A queued node is mutated only through this list.
        unsafe {
            let node = &mut *head.as_ref().node.get();
            debug_assert!(head.as_ref().queued.load(Ordering::Acquire));
            debug_assert!(node.next.is_none());
            debug_assert!(node.func.is_none());
            debug_assert!(node.target_gp.is_none());
            debug_assert!(node.callback_seq.is_none());

            node.func = Some(func);
            node.target_gp = Some(target_gp);
            node.callback_seq = Some(callback_seq);

            if let Some(tail) = self.tail {
                let tail_node = &mut *tail.as_ref().node.get();
                debug_assert!(tail_node.next.is_none());
                tail_node.next = Some(head);
            } else {
                debug_assert!(self.head.is_none());
                self.head = Some(head);
            }
        }

        self.tail = Some(head);
        self.len = self
            .len
            .checked_add(1)
            .expect("RCU callback count overflow");
        self.assert_invariants();
    }

    pub(super) fn pop_ready(&mut self, completed_gp: RcuSequence) -> Option<ReadyRcuCallback> {
        let head = self.head?;
        // SAFETY: The global RCU state lock serializes the list. The head is
        // still linked, so the admission lifetime contract keeps it valid.
        let node = unsafe { &mut *head.as_ref().node.get() };
        let target_gp = node
            .target_gp
            .expect("queued RCU callback has no target grace period");
        if !completed_gp.has_reached(target_gp) {
            return None;
        }

        self.head = node.next.take();
        if self.head.is_none() {
            self.tail = None;
        }
        self.len -= 1;

        let func = node
            .func
            .take()
            .expect("queued RCU callback has no function");
        let seq = node
            .callback_seq
            .take()
            .expect("queued RCU callback has no admission sequence");
        node.target_gp = None;

        // Publish complete detachment before the callback receives ownership.
        // After this store the drainer must not dereference `head` again.
        unsafe { head.as_ref() }
            .queued
            .store(false, Ordering::Release);
        self.assert_invariants();

        Some(ReadyRcuCallback { head, func, seq })
    }

    fn assert_invariants(&self) {
        debug_assert_eq!(self.head.is_none(), self.tail.is_none());
        debug_assert_eq!(self.len == 0, self.head.is_none());
    }
}

// SAFETY: The list's raw pointers are non-owning tokens transferred between
// CPUs only as part of `RcuStateInner`. Every dereference is serialized by its
// global spin lock, and admission guarantees stable pointee addresses.
unsafe impl Send for RcuCallbackList {}
