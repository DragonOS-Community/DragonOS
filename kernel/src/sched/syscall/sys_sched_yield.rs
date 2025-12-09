use system_error::SystemError;

use crate::arch::cpu::current_cpu_id;
use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_SCHED_YIELD;
use crate::exception::InterruptArch;
use crate::process::ProcessManager;
use crate::sched::fair::CompletelyFairScheduler;
use crate::sched::CurrentIrqArch;
use crate::sched::{cpu_rq, schedule, SchedMode, Scheduler};
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;
use alloc::vec::Vec;

/// System call handler for the `sched_yield` syscall
///
/// This handler implements the `Syscall` trait to provide functionality for
/// voluntarily yielding the CPU to other processes.
struct SysSchedYield;

impl Syscall for SysSchedYield {
    /// Returns the number of arguments expected by the `sched_yield` syscall
    fn num_args(&self) -> usize {
        0
    }

    /// Handles the `sched_yield` system call
    ///
    /// Voluntarily yields the CPU to other processes. The current process
    /// will be moved to the end of the run queue for its priority level.
    ///
    /// # Arguments
    /// * `_args` - Array containing no arguments (unused)
    /// * `_frame` - Trap frame (unused in this implementation)
    ///
    /// # Returns
    /// * `Ok(0)`: Success
    fn handle(&self, _args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
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

    /// Formats the syscall parameters for display/debug purposes
    ///
    /// # Arguments
    /// * `_args` - The raw syscall arguments (unused, as sched_yield takes no arguments)
    ///
    /// # Returns
    /// Empty vector (sched_yield takes no arguments)
    fn entry_format(&self, _args: &[usize]) -> Vec<FormattedSyscallParam> {
        Vec::new()
    }
}

syscall_table_macros::declare_syscall!(SYS_SCHED_YIELD, SysSchedYield);
