use system_error::SystemError;

use crate::arch::cpu::current_cpu_id;
use crate::exception::InterruptArch;
use crate::process::ProcessManager;
use crate::sched::CurrentIrqArch;
use crate::sched::Scheduler;
use crate::syscall::Syscall;

use super::fair::CompletelyFairScheduler;
use super::{cpu_rq, schedule, SchedMode};

impl Syscall {
    pub fn do_sched_yield() -> Result<usize, SystemError> {
        // 禁用中断
        let irq_guard = unsafe { CurrentIrqArch::save_and_disable_irq() };

        let pcb = ProcessManager::current_pcb();
        let rq = cpu_rq(pcb.sched_info().on_cpu().unwrap_or(current_cpu_id()).data() as usize);
        let (rq, guard) = rq.self_lock();

        // TODO: schedstat_inc(rq->yld_count);

        CompletelyFairScheduler::yield_task(rq);

        pcb.preempt_disable();

        drop(guard);
        drop(irq_guard);

        pcb.preempt_enable(); // sched_preempt_enable_no_resched();

        schedule(SchedMode::SM_NONE);

        Ok(0)
    }
}
