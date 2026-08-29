use core::{
    cell::UnsafeCell,
    ptr::NonNull,
    sync::atomic::{AtomicBool, Ordering},
};

use super::{RcuRawCallback, gp::RcuSequence};

struct RcuHeadNode {
    next: Option<NonNull<RcuHead>>,
    func: Option<RcuRawCallback>,
}

impl RcuHeadNode {
    const fn new() -> Self {
        Self {
            next: None,
            func: None,
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

// SAFETY: `queued` is atomic. All access to `node` is serialized by a
// per-CPU callback-state lock, and admission keeps a queued head stable.
unsafe impl Send for RcuHead {}
unsafe impl Sync for RcuHead {}

pub(super) struct ReadyRcuCallback {
    pub(super) head: NonNull<RcuHead>,
    pub(super) func: RcuRawCallback,
}

/// One intrusive FIFO used as a segment of a per-CPU callback queue.
struct RcuCallbackList {
    head: Option<NonNull<RcuHead>>,
    tail: Option<NonNull<RcuHead>>,
    len: usize,
}

impl RcuCallbackList {
    const fn new() -> Self {
        Self {
            head: None,
            tail: None,
            len: 0,
        }
    }

    fn is_empty(&self) -> bool {
        self.head.is_none()
    }

    fn len(&self) -> usize {
        self.len
    }

    fn push(&mut self, head: NonNull<RcuHead>, func: RcuRawCallback) {
        // SAFETY: The containing state lock is held, `head` is exclusively
        // claimed, and its address remains stable until callback start.
        unsafe {
            let node = &mut *head.as_ref().node.get();
            debug_assert!(head.as_ref().queued.load(Ordering::Acquire));
            debug_assert!(node.next.is_none());
            debug_assert!(node.func.is_none());
            node.func = Some(func);

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

    fn pop(&mut self) -> Option<ReadyRcuCallback> {
        let head = self.head?;
        // SAFETY: The containing state lock serializes list access and the
        // admission contract keeps every linked head valid.
        let node = unsafe { &mut *head.as_ref().node.get() };
        self.head = node.next.take();
        if self.head.is_none() {
            self.tail = None;
        }
        self.len -= 1;
        let func = node
            .func
            .take()
            .expect("queued RCU callback has no function");

        // Publish complete detachment before callback ownership starts.
        unsafe { head.as_ref() }
            .queued
            .store(false, Ordering::Release);
        self.assert_invariants();
        Some(ReadyRcuCallback { head, func })
    }

    /// Appends `source` as one whole FIFO and leaves it empty.
    fn append(&mut self, source: &mut Self) {
        if source.is_empty() {
            return;
        }

        // SAFETY: Both lists are exclusively protected, and their nodes are
        // disjoint because a head can be queued only once.
        unsafe {
            if let Some(tail) = self.tail {
                (*tail.as_ref().node.get()).next = source.head;
            } else {
                self.head = source.head;
            }
        }
        self.tail = source.tail;
        self.len = self
            .len
            .checked_add(source.len)
            .expect("RCU callback count overflow");
        source.head = None;
        source.tail = None;
        source.len = 0;
        self.assert_invariants();
        source.assert_invariants();
    }

    fn assert_invariants(&self) {
        debug_assert_eq!(self.head.is_none(), self.tail.is_none());
        debug_assert_eq!(self.len == 0, self.head.is_none());
    }
}

// SAFETY: Raw links are dereferenced only while the containing state lock is
// held, and admission guarantees stable pointee addresses.
unsafe impl Send for RcuCallbackList {}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct RcuCallbackQueueDepth {
    pub(crate) done: usize,
    pub(crate) wait: usize,
    pub(crate) next_ready: usize,
    pub(crate) next: usize,
    pub(crate) executing: bool,
}

impl RcuCallbackQueueDepth {
    pub(crate) fn total(self) -> usize {
        self.done + self.wait + self.next_ready + self.next
    }

    pub(super) fn add_assign(&mut self, other: Self) {
        self.done += other.done;
        self.wait += other.wait;
        self.next_ready += other.next_ready;
        self.next += other.next;
        self.executing |= other.executing;
    }
}

/// Four explicit grace-period states for one CPU's callbacks.
pub(super) struct RcuSegmentedCallbacks {
    done: RcuCallbackList,
    wait: RcuCallbackList,
    next_ready: RcuCallbackList,
    next: RcuCallbackList,
    wait_target: Option<RcuSequence>,
    next_ready_target: Option<RcuSequence>,
}

impl RcuSegmentedCallbacks {
    pub(super) const fn new() -> Self {
        Self {
            done: RcuCallbackList::new(),
            wait: RcuCallbackList::new(),
            next_ready: RcuCallbackList::new(),
            next: RcuCallbackList::new(),
            wait_target: None,
            next_ready_target: None,
        }
    }

    pub(super) fn enqueue(&mut self, head: NonNull<RcuHead>, func: RcuRawCallback) {
        self.next.push(head, func);
        self.assert_invariants();
    }

    pub(super) fn has_ready(&self) -> bool {
        !self.done.is_empty()
    }

    pub(super) fn has_unclassified(&self) -> bool {
        !self.next.is_empty()
    }

    pub(super) fn is_empty(&self) -> bool {
        self.depth().total() == 0
    }

    pub(super) fn depth(&self) -> RcuCallbackQueueDepth {
        RcuCallbackQueueDepth {
            done: self.done.len(),
            wait: self.wait.len(),
            next_ready: self.next_ready.len(),
            next: self.next.len(),
            executing: false,
        }
    }

    pub(super) fn classify_next(&mut self, target: RcuSequence, gp_active: bool) -> bool {
        if self.next.is_empty() {
            return false;
        }
        if gp_active {
            Self::set_or_check_target(&mut self.next_ready_target, target);
            self.next_ready.append(&mut self.next);
        } else {
            Self::set_or_check_target(&mut self.wait_target, target);
            self.wait.append(&mut self.next);
        }
        self.assert_invariants();
        true
    }

    /// Absorbs callbacks known to precede an imminent GP start.
    pub(super) fn prepare_gp_start(&mut self, seq: RcuSequence) {
        // `next_ready` predates `next`: it was classified while the previous
        // GP was active, whereas `next` contains later, not-yet-classified
        // admissions. Preserve that global FIFO order when both become
        // covered by this imminent GP.
        if !self.next_ready.is_empty() {
            debug_assert_eq!(self.next_ready_target, Some(seq));
            Self::set_or_check_target(&mut self.wait_target, seq);
            self.wait.append(&mut self.next_ready);
            self.next_ready_target = None;
        }
        if !self.next.is_empty() {
            Self::set_or_check_target(&mut self.wait_target, seq);
            self.wait.append(&mut self.next);
        }
        self.assert_invariants();
    }

    pub(super) fn complete_gp(&mut self, completed: RcuSequence) -> bool {
        if self.wait.is_empty() {
            return false;
        }
        let target = self
            .wait_target
            .expect("non-empty RCU wait segment has no GP");
        if !completed.has_reached(target) {
            return false;
        }
        self.done.append(&mut self.wait);
        self.wait_target = None;
        self.assert_invariants();
        true
    }

    pub(super) fn pop_ready(&mut self) -> Option<ReadyRcuCallback> {
        let callback = self.done.pop();
        self.assert_invariants();
        callback
    }

    /// Entrains a barrier marker after the last currently queued callback.
    pub(super) fn entrain(&mut self, head: NonNull<RcuHead>, func: RcuRawCallback) -> bool {
        if !self.next.is_empty() {
            self.next.push(head, func);
        } else if !self.next_ready.is_empty() {
            self.next_ready.push(head, func);
        } else if !self.wait.is_empty() {
            self.wait.push(head, func);
        } else if !self.done.is_empty() {
            self.done.push(head, func);
        } else {
            return false;
        }
        self.assert_invariants();
        true
    }

    pub(super) fn push_done(&mut self, head: NonNull<RcuHead>, func: RcuRawCallback) {
        self.done.push(head, func);
        self.assert_invariants();
    }

    pub(super) fn merge_from(&mut self, source: &mut Self) {
        Self::merge_target_segment(
            &mut self.wait,
            &mut self.wait_target,
            &mut source.wait,
            &mut source.wait_target,
        );
        Self::merge_target_segment(
            &mut self.next_ready,
            &mut self.next_ready_target,
            &mut source.next_ready,
            &mut source.next_ready_target,
        );
        self.done.append(&mut source.done);
        self.next.append(&mut source.next);
        self.assert_invariants();
        source.assert_invariants();
    }

    fn merge_target_segment(
        destination: &mut RcuCallbackList,
        destination_target: &mut Option<RcuSequence>,
        source: &mut RcuCallbackList,
        source_target: &mut Option<RcuSequence>,
    ) {
        if source.is_empty() {
            return;
        }
        let target = source_target.expect("non-empty RCU segment has no GP target");
        Self::set_or_check_target(destination_target, target);
        destination.append(source);
        *source_target = None;
    }

    fn set_or_check_target(slot: &mut Option<RcuSequence>, target: RcuSequence) {
        if let Some(current) = *slot {
            assert_eq!(current, target, "merged incompatible RCU callback segments");
        } else {
            *slot = Some(target);
        }
    }

    fn assert_invariants(&self) {
        debug_assert_eq!(self.wait.is_empty(), self.wait_target.is_none());
        debug_assert_eq!(self.next_ready.is_empty(), self.next_ready_target.is_none());
        self.done.assert_invariants();
        self.wait.assert_invariants();
        self.next_ready.assert_invariants();
        self.next.assert_invariants();
    }
}

// SAFETY: The segmented queue is accessed only through its containing
// per-CPU callback-state lock.
unsafe impl Send for RcuSegmentedCallbacks {}

pub(super) fn run_segmented_callback_selftests() -> Result<(), &'static str> {
    unsafe fn noop(_head: NonNull<RcuHead>) {}

    let heads = [const { RcuHead::new() }; 5];
    for head in &heads {
        if !head.try_claim() {
            return Err("fresh segmented callback head could not be claimed");
        }
    }

    let gp1 = RcuSequence::from_raw(1);
    let gp2 = RcuSequence::from_raw(2);
    let gp3 = RcuSequence::from_raw(3);
    let gp4 = RcuSequence::from_raw(4);
    let mut source = RcuSegmentedCallbacks::new();
    let mut destination = RcuSegmentedCallbacks::new();

    source.enqueue(NonNull::from(&heads[0]), noop);
    source.classify_next(gp1, false);
    source.complete_gp(gp1);
    source.enqueue(NonNull::from(&heads[1]), noop);
    source.classify_next(gp2, false);
    source.enqueue(NonNull::from(&heads[2]), noop);
    source.classify_next(gp3, true);
    source.enqueue(NonNull::from(&heads[3]), noop);

    if source.depth()
        != (RcuCallbackQueueDepth {
            done: 1,
            wait: 1,
            next_ready: 1,
            next: 1,
            executing: false,
        })
    {
        return Err("could not construct all four RCU callback segments");
    }

    destination.merge_from(&mut source);
    if !source.is_empty() || destination.depth().total() != 4 {
        return Err("RCU callback migration did not move every segment");
    }
    if !destination.entrain(NonNull::from(&heads[4]), noop) {
        return Err("RCU barrier marker could not entrain behind pending callbacks");
    }

    if destination.pop_ready().map(|callback| callback.head) != Some(NonNull::from(&heads[0])) {
        return Err("RCU done segment lost FIFO order during migration");
    }
    destination.complete_gp(gp2);
    if destination.pop_ready().map(|callback| callback.head) != Some(NonNull::from(&heads[1])) {
        return Err("RCU wait segment did not advance as a whole");
    }
    destination.prepare_gp_start(gp3);
    destination.complete_gp(gp3);
    if destination.pop_ready().map(|callback| callback.head) != Some(NonNull::from(&heads[2])) {
        return Err("RCU next-ready segment did not advance as a whole");
    }
    destination.classify_next(gp4, false);
    destination.complete_gp(gp4);
    if destination.pop_ready().map(|callback| callback.head) != Some(NonNull::from(&heads[3]))
        || destination.pop_ready().map(|callback| callback.head) != Some(NonNull::from(&heads[4]))
        || !destination.is_empty()
    {
        return Err("RCU barrier marker did not remain behind its queue prefix");
    }

    Ok(())
}
