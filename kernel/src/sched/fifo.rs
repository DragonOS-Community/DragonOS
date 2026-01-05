use alloc::{collections::VecDeque, sync::Arc, vec::Vec};

use crate::{process::ProcessControlBlock, sched::prio::MAX_RT_PRIO};

use super::{CpuRunQueue, DequeueFlag, EnqueueFlag, PrioUtil, SchedPolicy, Scheduler, WakeupFlags};

#[derive(Debug)]
pub struct FifoRunQueue {
    queues: Vec<VecDeque<Arc<ProcessControlBlock>>>,
    active: u128,
    nr_running: usize,
}

impl FifoRunQueue {
    pub fn new() -> Self {
        let mut queues = Vec::with_capacity(MAX_RT_PRIO as usize);
        queues.resize_with(MAX_RT_PRIO as usize, VecDeque::new);
        Self {
            queues,
            active: 0,
            nr_running: 0,
        }
    }

    #[inline]
    pub fn nr_running(&self) -> usize {
        self.nr_running
    }

    #[inline]
    fn prio_index(pcb: &ProcessControlBlock) -> usize {
        let prio = pcb.sched_info().prio_data.read_irqsave().prio;
        let prio = prio.clamp(0, MAX_RT_PRIO - 1);
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

    pub fn enqueue(&mut self, pcb: Arc<ProcessControlBlock>) {
        let prio = Self::prio_index(&pcb);
        self.queues[prio].push_back(pcb);
        self.set_active(prio);
        self.nr_running += 1;
    }

    pub fn dequeue(&mut self, pcb: &Arc<ProcessControlBlock>) -> bool {
        let prio = Self::prio_index(pcb);
        let q = &mut self.queues[prio];
        let before = q.len();
        q.retain(|p| !Arc::ptr_eq(p, pcb));
        let removed = before != q.len();
        if removed {
            self.nr_running -= 1;
            self.clear_active_if_empty(prio);
        }
        removed
    }

    pub fn yield_current(&mut self, pcb: &Arc<ProcessControlBlock>) {
        let prio = Self::prio_index(pcb);
        let q = &mut self.queues[prio];
        if q.len() <= 1 {
            return;
        }
        q.retain(|p| !Arc::ptr_eq(p, pcb));
        q.push_back(pcb.clone());
        self.set_active(prio);
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

pub struct FifoScheduler;

impl FifoScheduler {
    #[inline]
    fn rt_prio(pcb: &ProcessControlBlock) -> i32 {
        pcb.sched_info().prio_data.read_irqsave().prio
    }
}

impl Scheduler for FifoScheduler {
    fn enqueue(rq: &mut CpuRunQueue, pcb: Arc<ProcessControlBlock>, _flags: EnqueueFlag) {
        rq.fifo.enqueue(pcb);
        rq.add_nr_running(1);
    }

    fn dequeue(rq: &mut CpuRunQueue, pcb: Arc<ProcessControlBlock>, _flags: DequeueFlag) {
        if rq.fifo.dequeue(&pcb) {
            rq.sub_nr_running(1);
        }
    }

    fn yield_task(rq: &mut CpuRunQueue) {
        let curr = rq.current();
        if curr.sched_info().policy() != SchedPolicy::FIFO {
            return;
        }
        rq.fifo.yield_current(&curr);
        rq.resched_current();
    }

    fn check_preempt_currnet(
        rq: &mut CpuRunQueue,
        pcb: &Arc<ProcessControlBlock>,
        _flags: WakeupFlags,
    ) {
        let curr = rq.current();
        if curr.sched_info().policy() != SchedPolicy::FIFO {
            rq.resched_current();
            return;
        }

        let new_prio = Self::rt_prio(pcb);
        let curr_prio = Self::rt_prio(&curr);
        if PrioUtil::rt_prio(new_prio) && PrioUtil::rt_prio(curr_prio) && new_prio < curr_prio {
            rq.resched_current();
        }
    }

    fn pick_task(rq: &mut CpuRunQueue) -> Option<Arc<ProcessControlBlock>> {
        FifoScheduler::pick_next_task(rq, None)
    }

    fn pick_next_task(
        rq: &mut CpuRunQueue,
        _pcb: Option<Arc<ProcessControlBlock>>,
    ) -> Option<Arc<ProcessControlBlock>> {
        rq.fifo.pick_next()
    }

    fn tick(rq: &mut CpuRunQueue, pcb: Arc<ProcessControlBlock>, _queued: bool) {
        if pcb.sched_info().policy() != SchedPolicy::FIFO {
            rq.resched_current();
            return;
        }

        let Some(highest) = rq.fifo.highest_prio() else {
            return;
        };

        let curr_prio = Self::rt_prio(&pcb);
        if PrioUtil::rt_prio(curr_prio) && (highest as i32) < curr_prio {
            rq.resched_current();
        }
    }

    fn task_fork(_pcb: Arc<ProcessControlBlock>) {}

    fn put_prev_task(_rq: &mut CpuRunQueue, _prev: Arc<ProcessControlBlock>) {}
}
