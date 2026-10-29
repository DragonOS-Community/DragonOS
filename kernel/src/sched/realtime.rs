use alloc::{boxed::Box, sync::Arc};
use core::ptr::NonNull;

use crate::{
    process::ProcessControlBlock,
    sched::{prio::MAX_RT_PRIO, LinuxSchedPolicy},
    time::clocksource::HZ,
};

use super::{CpuRunQueue, DequeueFlag, EnqueueFlag, PrioUtil, SchedClass, Scheduler, WakeupFlags};

/// Match Linux's default root realtime bandwidth: reserve 5% of each CPU for
/// non-RT work in every one-second period.
const RT_PERIOD_NS: u64 = 1_000_000_000;
const RT_RUNTIME_NS: u64 = 950_000_000;

/// Linux's default SCHED_RR quantum, expressed in scheduler ticks.
pub(crate) const RR_TIMESLICE_TICKS: u32 = (100 * HZ / 1000) as u32;

const _: () = {
    assert!(MAX_RT_PRIO > 1);
    assert!(MAX_RT_PRIO <= u128::BITS as i32);
    assert!(RR_TIMESLICE_TICKS > 0);
};

/// One allocation per task, prepared before publication. Detached nodes have
/// no task payload, so the task's spare cannot hold a strong self-reference.
#[derive(Debug)]
pub(crate) struct RtQueueNode {
    task: Option<Arc<ProcessControlBlock>>,
    next: Option<Box<RtQueueNode>>,
}

impl RtQueueNode {
    pub(crate) fn try_new() -> Result<Box<Self>, core::alloc::AllocError> {
        Box::try_new(Self {
            task: None,
            next: None,
        })
    }
}

impl Drop for RtQueueNode {
    fn drop(&mut self) {
        let mut next = self.next.take();
        while let Some(mut node) = next {
            next = node.next.take();
        }
    }
}

#[derive(Debug, Default)]
struct RtBucket {
    head: Option<Box<RtQueueNode>>,
    /// Non-owning pointer to the last Box in head's chain. It never escapes
    /// this bucket and is accessed only through exclusive owner-rq mutation.
    tail: Option<NonNull<RtQueueNode>>,
    len: usize,
}

// Moving an exclusively owned bucket moves Box handles, not their pointees.
// Its only raw pointer still names that same owned chain; shared mutation is
// serialized by the existing runqueue lock, never by the pointer itself.
unsafe impl Send for RtBucket {}
unsafe impl Sync for RtBucket {}

impl RtBucket {
    fn is_empty(&self) -> bool {
        self.head.is_none()
    }

    fn len(&self) -> usize {
        self.len
    }

    fn front(&self) -> Option<&Arc<ProcessControlBlock>> {
        self.head.as_ref().and_then(|node| node.task.as_ref())
    }

    fn iter(&self) -> impl Iterator<Item = &Arc<ProcessControlBlock>> {
        let mut current = self.head.as_deref();
        core::iter::from_fn(move || {
            let node = current?;
            current = node.next.as_deref();
            Some(node.task.as_ref().expect("queued RT node has no task"))
        })
    }

    fn push_back(&mut self, mut node: Box<RtQueueNode>) {
        debug_assert!(node.next.is_none() && node.task.is_some());
        let address = NonNull::from(&mut *node);
        if let Some(mut tail) = self.tail {
            // SAFETY: tail points to the live final Box exclusively owned by
            // this chain, and exclusive &mut self prevents concurrent access.
            unsafe { tail.as_mut().next = Some(node) };
        } else {
            debug_assert!(self.head.is_none());
            self.head = Some(node);
        }
        self.tail = Some(address);
        self.len += 1;
    }

    fn push_front(&mut self, mut node: Box<RtQueueNode>) {
        debug_assert!(node.next.is_none() && node.task.is_some());
        if self.head.is_none() {
            debug_assert!(self.tail.is_none());
            self.tail = Some(NonNull::from(&mut *node));
        }
        node.next = self.head.take();
        self.head = Some(node);
        self.len += 1;
    }

    fn remove(&mut self, task: &Arc<ProcessControlBlock>) -> Option<Box<RtQueueNode>> {
        let mut link = &mut self.head;
        let mut previous = None;
        while let Some(node) = link.as_ref() {
            if node
                .task
                .as_ref()
                .is_some_and(|queued| Arc::ptr_eq(queued, task))
            {
                let mut removed = link.take().unwrap();
                let address = NonNull::from(&mut *removed);
                *link = removed.next.take();
                if self.tail == Some(address) {
                    self.tail = previous;
                }
                self.len -= 1;
                return Some(removed);
            }
            let node = link.as_deref_mut().unwrap();
            previous = Some(NonNull::from(&mut *node));
            link = &mut node.next;
        }
        None
    }

    fn is_tail(&self, task: &Arc<ProcessControlBlock>) -> bool {
        self.tail.is_some_and(|tail| {
            // SAFETY: immutable access neither removes nor moves any owned
            // node. The last node is kept live by head for the entire borrow.
            unsafe {
                tail.as_ref()
                    .task
                    .as_ref()
                    .is_some_and(|queued| Arc::ptr_eq(queued, task))
            }
        })
    }
}

/// Per-CPU runqueue shared by every policy in the realtime scheduling class.
///
/// Lower numeric priorities run first. Tasks at the same priority keep FIFO
/// order unless the class explicitly moves an existing task to the tail.
#[derive(Debug)]
pub struct RealtimeRunQueue {
    queues: [RtBucket; MAX_RT_PRIO as usize],
    active: u128,
    nr_running: usize,
    runtime_used: u64,
    period_start: Option<u64>,
    exec_start: Option<u64>,
    throttled: bool,
}

impl RealtimeRunQueue {
    pub fn new() -> Self {
        Self {
            queues: core::array::from_fn(|_| RtBucket::default()),
            active: 0,
            nr_running: 0,
            runtime_used: 0,
            period_start: None,
            exec_start: None,
            throttled: false,
        }
    }

    #[inline]
    pub fn nr_running(&self) -> usize {
        self.nr_running
    }

    #[inline]
    pub fn is_throttled(&self) -> bool {
        self.throttled
    }

    /// Account the current RT task before advancing the bandwidth period.
    ///
    /// This order preserves runtime debt when a non-preemptible kernel path
    /// crosses one or more period boundaries before reaching the scheduler.
    fn update_current(&mut self, clock_task: u64, clock: u64) -> u64 {
        let mut delta = 0;
        if let Some(exec_start) = self.exec_start {
            delta = clock_task.saturating_sub(exec_start);
            self.runtime_used = self.runtime_used.saturating_add(delta);
            self.exec_start = Some(clock_task);
        }
        self.update_period(clock);
        delta
    }

    fn update_period(&mut self, clock: u64) {
        let Some(period_start) = self.period_start.as_mut() else {
            self.period_start = Some(clock - clock % RT_PERIOD_NS);
            self.update_throttled();
            return;
        };
        let elapsed = clock.saturating_sub(*period_start);

        if elapsed >= RT_PERIOD_NS {
            let elapsed_periods = elapsed / RT_PERIOD_NS;
            *period_start =
                period_start.saturating_add(elapsed_periods.saturating_mul(RT_PERIOD_NS));
            self.runtime_used = self
                .runtime_used
                .saturating_sub(elapsed_periods.saturating_mul(RT_RUNTIME_NS));
        }

        self.update_throttled();
    }

    fn update_throttled(&mut self) {
        // Linux uses hysteresis at the exact runtime boundary: `>` enters
        // throttling, while an already-throttled rq needs `<` to leave it.
        if self.throttled {
            if self.runtime_used < RT_RUNTIME_NS {
                self.throttled = false;
            }
        } else if self.runtime_used > RT_RUNTIME_NS {
            self.throttled = true;
        }
    }

    #[inline]
    fn set_current(&mut self, clock_task: u64) {
        self.exec_start = Some(clock_task);
    }

    #[inline]
    fn clear_current(&mut self) {
        self.exec_start = None;
    }

    #[inline]
    fn prio_index(pcb: &ProcessControlBlock) -> usize {
        let prio = pcb.sched_info().prio();
        assert!(
            (0..MAX_RT_PRIO - 1).contains(&prio),
            "realtime task has invalid internal priority {prio}"
        );
        prio as usize
    }

    #[inline]
    fn set_active(&mut self, prio: usize) {
        self.active |= 1u128 << prio;
    }

    #[inline]
    fn clear_active_if_empty(&mut self, prio: usize) {
        if self.queues[prio].is_empty() {
            self.active &= !(1u128 << prio);
        }
    }

    #[inline]
    fn assert_not_queued(&self, _pcb: &Arc<ProcessControlBlock>) {
        #[cfg(any(debug_assertions, feature = "fifo_demo"))]
        assert!(
            !self
                .queues
                .iter()
                .any(|queue| queue.iter().any(|queued| Arc::ptr_eq(queued, _pcb))),
            "realtime task is already queued"
        );
    }

    #[inline]
    fn assert_consistent(&self) {
        #[cfg(any(debug_assertions, feature = "fifo_demo"))]
        {
            let mut expected_active = 0u128;
            let mut expected_running = 0usize;

            for (prio, queue) in self.queues.iter().enumerate() {
                if !queue.is_empty() {
                    expected_active |= 1u128 << prio;
                }
                expected_running += queue.len();
                for task in queue.iter() {
                    assert_eq!(
                        Self::prio_index(task),
                        prio,
                        "realtime task is queued in the wrong priority bucket"
                    );
                }
            }

            assert_eq!(self.active, expected_active, "realtime bitmap mismatch");
            assert_eq!(
                self.nr_running, expected_running,
                "realtime running count mismatch"
            );
        }
    }

    pub fn enqueue_tail(&mut self, pcb: Arc<ProcessControlBlock>) {
        let prio = Self::prio_index(&pcb);
        self.assert_not_queued(&pcb);
        let mut node = pcb.sched_info().take_rt_node();
        node.task = Some(pcb);
        self.queues[prio].push_back(node);
        self.set_active(prio);
        self.nr_running += 1;
        self.assert_consistent();
    }

    pub fn enqueue_head(&mut self, pcb: Arc<ProcessControlBlock>) {
        let prio = Self::prio_index(&pcb);
        self.assert_not_queued(&pcb);
        let mut node = pcb.sched_info().take_rt_node();
        node.task = Some(pcb);
        self.queues[prio].push_front(node);
        self.set_active(prio);
        self.nr_running += 1;
        self.assert_consistent();
    }

    pub fn dequeue(&mut self, pcb: &Arc<ProcessControlBlock>) -> bool {
        self.assert_consistent();
        let prio = Self::prio_index(pcb);
        let node = self.queues[prio].remove(pcb);

        #[cfg(any(debug_assertions, feature = "fifo_demo"))]
        assert!(node.is_some(), "realtime task is not queued");

        let Some(mut node) = node else {
            return false;
        };

        let payload = node.task.take().expect("queued RT node has no task");
        pcb.sched_info().return_rt_node(node);
        // The caller's Arc pins pcb. Drop only after the spare guard is gone.
        drop(payload);
        self.nr_running -= 1;
        self.clear_active_if_empty(prio);
        self.assert_consistent();
        true
    }

    /// Move an existing task to the tail of its current priority bucket.
    pub fn requeue_to_tail(&mut self, pcb: &Arc<ProcessControlBlock>) -> bool {
        self.assert_consistent();
        let prio = Self::prio_index(pcb);
        if self.queues[prio].is_tail(pcb) {
            return true;
        }
        let node = self.queues[prio].remove(pcb);
        #[cfg(any(debug_assertions, feature = "fifo_demo"))]
        assert!(node.is_some(), "realtime task is not queued");
        let Some(node) = node else {
            return false;
        };

        self.queues[prio].push_back(node);
        self.assert_consistent();
        true
    }

    /// Move an existing task to the tail only when its priority bucket has a
    /// peer which can run next.
    fn requeue_to_tail_if_peer(&mut self, pcb: &Arc<ProcessControlBlock>) -> bool {
        self.assert_consistent();
        let prio = Self::prio_index(pcb);

        #[cfg(any(debug_assertions, feature = "fifo_demo"))]
        assert!(
            self.queues[prio]
                .iter()
                .any(|queued| Arc::ptr_eq(queued, pcb)),
            "realtime task is not queued"
        );

        if self.queues[prio].len() <= 1 {
            return false;
        }
        self.requeue_to_tail(pcb)
    }

    pub fn pick_next(&self) -> Option<Arc<ProcessControlBlock>> {
        let prio = self.highest_prio()?;
        self.queues[prio].front().cloned()
    }

    pub fn highest_prio(&self) -> Option<usize> {
        if self.active == 0 {
            return None;
        }
        Some(self.active.trailing_zeros() as usize)
    }
}

pub struct RealtimeScheduler;

impl RealtimeScheduler {
    #[inline]
    fn rt_prio(pcb: &ProcessControlBlock) -> i32 {
        pcb.sched_info().prio()
    }

    /// Bring the local RT bandwidth state up to date under the rq lock.
    pub fn update_bandwidth(rq: &mut CpuRunQueue, current_class: SchedClass) {
        let was_throttled = rq.rt.is_throttled();
        if current_class == SchedClass::Realtime {
            let delta = rq.rt.update_current(rq.clock_task, rq.clock);
            let task = rq.current();
            task.add_sum_exec_runtime(delta);
            task.account_cgroup_runtime(delta);
        } else {
            rq.rt.update_period(rq.clock);
        }

        if !was_throttled && rq.rt.is_throttled() {
            // Match Linux's dequeue_top_rt_rq(): throttled RT tasks stay on
            // their class queue but no longer contribute to the top-level rq.
            let nr_running = rq.rt.nr_running();
            rq.sub_nr_running(nr_running);
            rq.resched_current();
        } else if was_throttled && !rq.rt.is_throttled() {
            let nr_running = rq.rt.nr_running();
            rq.add_nr_running(nr_running);
            if nr_running > 0 {
                rq.resched_current();
            }
        }
    }

    /// Start charging the selected realtime task on this runqueue.
    pub fn set_next_task(
        rq: &mut super::CpuRunQueue,
        _pcb: alloc::sync::Arc<crate::process::ProcessControlBlock>,
    ) {
        rq.rt.set_current(rq.clock_task);
        if rq.rt.is_throttled() {
            // This is reachable when a running Fair task is changed to RT on
            // an already-throttled rq. Charge its bounded tail, then switch it
            // out instead of letting the policy transaction bypass bandwidth.
            rq.resched_current();
        }
    }
}

impl Scheduler for RealtimeScheduler {
    fn enqueue(rq: &mut CpuRunQueue, pcb: Arc<ProcessControlBlock>, flags: EnqueueFlag) {
        let current_class = rq.current().sched_info().sched_class();
        Self::update_bandwidth(rq, current_class);
        if flags.contains(EnqueueFlag::ENQUEUE_HEAD) {
            rq.rt.enqueue_head(pcb);
        } else {
            rq.rt.enqueue_tail(pcb);
        }
        if !rq.rt.is_throttled() {
            rq.add_nr_running(1);
        }
    }

    fn dequeue(rq: &mut CpuRunQueue, pcb: Arc<ProcessControlBlock>, _flags: DequeueFlag) {
        if rq.rt.dequeue(&pcb) && !rq.rt.is_throttled() {
            rq.sub_nr_running(1);
        }
    }

    fn yield_task(rq: &mut CpuRunQueue) {
        let curr = rq.current();
        debug_assert_eq!(curr.sched_info().sched_class(), SchedClass::Realtime);
        if rq.rt.requeue_to_tail(&curr) {
            rq.resched_current();
        }
    }

    fn check_preempt_current(
        rq: &mut CpuRunQueue,
        pcb: &Arc<ProcessControlBlock>,
        _flags: WakeupFlags,
    ) {
        let curr = rq.current();
        debug_assert_eq!(curr.sched_info().sched_class(), SchedClass::Realtime);

        let new_prio = Self::rt_prio(pcb);
        let curr_prio = Self::rt_prio(&curr);
        if PrioUtil::rt_prio(new_prio) && PrioUtil::rt_prio(curr_prio) && new_prio < curr_prio {
            rq.resched_current();
        }
    }

    fn pick_task(rq: &mut CpuRunQueue) -> Option<Arc<ProcessControlBlock>> {
        RealtimeScheduler::pick_next_task(rq, None)
    }

    fn pick_next_task(
        rq: &mut CpuRunQueue,
        _pcb: Option<Arc<ProcessControlBlock>>,
    ) -> Option<Arc<ProcessControlBlock>> {
        if rq.rt.is_throttled() {
            return None;
        }
        rq.rt.pick_next()
    }

    fn tick(rq: &mut CpuRunQueue, pcb: Arc<ProcessControlBlock>, _queued: bool) {
        debug_assert_eq!(pcb.sched_info().sched_class(), SchedClass::Realtime);

        let Some(highest) = rq.rt.highest_prio() else {
            return;
        };

        let curr_prio = Self::rt_prio(&pcb);
        if PrioUtil::rt_prio(curr_prio) && (highest as i32) < curr_prio {
            rq.resched_current();
        }

        if pcb.sched_info().policy() != LinuxSchedPolicy::Rr {
            return;
        }

        let remaining = pcb.sched_info().rr_time_slice_remaining();
        debug_assert!(remaining > 0, "RR task has an empty time slice");
        if remaining > 1 {
            pcb.sched_info().set_rr_time_slice_remaining(remaining - 1);
            return;
        }

        // Linux reloads an exhausted slice even when this is the only task in
        // its priority bucket, but only rotates and reschedules when a peer can
        // run next.
        pcb.sched_info()
            .set_rr_time_slice_remaining(RR_TIMESLICE_TICKS);
        if rq.rt.requeue_to_tail_if_peer(&pcb) {
            rq.resched_current();
        }
    }

    fn task_fork(_pcb: Arc<ProcessControlBlock>) {}

    fn put_prev_task(rq: &mut CpuRunQueue, _prev: Arc<ProcessControlBlock>) {
        Self::update_bandwidth(rq, SchedClass::Realtime);
        rq.rt.clear_current();
    }
}
