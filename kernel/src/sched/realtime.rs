use alloc::{collections::VecDeque, sync::Arc, vec::Vec};

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

/// Per-CPU runqueue shared by every policy in the realtime scheduling class.
///
/// Lower numeric priorities run first. Tasks at the same priority keep FIFO
/// order unless the class explicitly moves an existing task to the tail.
#[derive(Debug)]
pub struct RealtimeRunQueue {
    queues: Vec<VecDeque<Arc<ProcessControlBlock>>>,
    active: u128,
    nr_running: usize,
    runtime_used: u64,
    period_start: Option<u64>,
    exec_start: Option<u64>,
    throttled: bool,
}

impl RealtimeRunQueue {
    pub fn new() -> Self {
        let mut queues = Vec::with_capacity(MAX_RT_PRIO as usize);
        queues.resize_with(MAX_RT_PRIO as usize, VecDeque::new);
        Self {
            queues,
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
    fn update_current(&mut self, clock_task: u64, clock: u64) {
        if let Some(exec_start) = self.exec_start {
            self.runtime_used = self
                .runtime_used
                .saturating_add(clock_task.saturating_sub(exec_start));
            self.exec_start = Some(clock_task);
        }
        self.update_period(clock);
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
                .flatten()
                .any(|queued| Arc::ptr_eq(queued, _pcb)),
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
                for task in queue {
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
        self.queues[prio].push_back(pcb);
        self.set_active(prio);
        self.nr_running += 1;
        self.assert_consistent();
    }

    pub fn enqueue_head(&mut self, pcb: Arc<ProcessControlBlock>) {
        let prio = Self::prio_index(&pcb);
        self.assert_not_queued(&pcb);
        self.queues[prio].push_front(pcb);
        self.set_active(prio);
        self.nr_running += 1;
        self.assert_consistent();
    }

    pub fn dequeue(&mut self, pcb: &Arc<ProcessControlBlock>) -> bool {
        self.assert_consistent();
        let prio = Self::prio_index(pcb);
        let position = self.queues[prio]
            .iter()
            .position(|queued| Arc::ptr_eq(queued, pcb));

        #[cfg(any(debug_assertions, feature = "fifo_demo"))]
        assert!(position.is_some(), "realtime task is not queued");

        let Some(position) = position else {
            return false;
        };

        self.queues[prio].remove(position);
        self.nr_running -= 1;
        self.clear_active_if_empty(prio);
        self.assert_consistent();
        true
    }

    /// Move an existing task to the tail of its current priority bucket.
    pub fn requeue_to_tail(&mut self, pcb: &Arc<ProcessControlBlock>) -> bool {
        self.assert_consistent();
        let prio = Self::prio_index(pcb);
        let position = self.queues[prio]
            .iter()
            .position(|queued| Arc::ptr_eq(queued, pcb));

        #[cfg(any(debug_assertions, feature = "fifo_demo"))]
        assert!(position.is_some(), "realtime task is not queued");

        let Some(position) = position else {
            return false;
        };

        if position + 1 != self.queues[prio].len() {
            let task = self.queues[prio]
                .remove(position)
                .expect("realtime queue position disappeared");
            self.queues[prio].push_back(task);
        }
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
            rq.rt.update_current(rq.clock_task, rq.clock);
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
