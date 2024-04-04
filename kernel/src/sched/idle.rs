use super::Scheduler;

pub struct IdleScheduler;

impl Scheduler for IdleScheduler {
    fn enqueue(
        _rq: &mut super::CpuRunQueue,
        _pcb: alloc::sync::Arc<crate::process::ProcessControlBlock>,
        _flags: super::EnqueueFlag,
    ) {
        // idle已经被设置，无需入队
    }

    fn dequeue(
        _rq: &mut super::CpuRunQueue,
        _pcb: alloc::sync::Arc<crate::process::ProcessControlBlock>,
        _flags: super::DequeueFlag,
    ) {
    }

    fn yield_task(_rq: &mut super::CpuRunQueue) {}

    fn check_preempt_currnet(
        rq: &mut super::CpuRunQueue,
        _pcb: &alloc::sync::Arc<crate::process::ProcessControlBlock>,
        _flags: super::WakeupFlags,
    ) {
        rq.resched_current();
    }

    fn pick_task(
        _rq: &mut super::CpuRunQueue,
    ) -> Option<alloc::sync::Arc<crate::process::ProcessControlBlock>> {
        panic!()
    }

    /// ### 表示idle被选中
    ///
    /// 主要做一些统计工作
    fn pick_next_task(
        _rq: &mut super::CpuRunQueue,
        _pcb: Option<alloc::sync::Arc<crate::process::ProcessControlBlock>>,
    ) -> Option<alloc::sync::Arc<crate::process::ProcessControlBlock>> {
        // TODO: Fixme
        // 做一些统计工作
        None
    }

    fn tick(
        _rq: &mut super::CpuRunQueue,
        _pcb: alloc::sync::Arc<crate::process::ProcessControlBlock>,
        _queued: bool,
    ) {
        // Nothing to do
    }

    fn task_fork(_pcb: alloc::sync::Arc<crate::process::ProcessControlBlock>) {
        todo!()
    }

    fn put_prev_task(
        _rq: &mut super::CpuRunQueue,
        _prev: alloc::sync::Arc<crate::process::ProcessControlBlock>,
    ) {
        // Nothing todo
    }
}
