use core::hint::spin_loop;

use log::error;

use crate::{
    arch::CurrentIrqArch,
    exception::InterruptArch,
    process::{ProcessFlags, ProcessManager},
    sched::{SchedMode, __schedule},
};

impl ProcessManager {
    /// 每个核的idle进程
    pub fn arch_idle_func() -> ! {
        loop {
            let pcb = ProcessManager::current_pcb();
            if pcb.flags().contains(ProcessFlags::NEED_SCHEDULE) {
                __schedule(SchedMode::SM_NONE);
            }
            if CurrentIrqArch::is_irq_enabled() {
                // Keep the final reschedule check and STI;HLT in one IRQ-off
                // region. A remote wakeup then either sets the flag before the
                // check or leaves an IPI pending to wake the halted CPU.
                let irq_guard = unsafe { CurrentIrqArch::save_and_disable_irq() };
                let rcu_idle = crate::rcu::idle_enter();
                // idle_enter() can itself wake a local RCU worker without an
                // IPI, so check after entering EQS as well as before it.
                if !pcb.flags().contains(ProcessFlags::NEED_SCHEDULE) {
                    unsafe { x86_64::instructions::interrupts::enable_and_hlt() };
                    unsafe { CurrentIrqArch::interrupt_disable() };
                }
                crate::rcu::idle_exit(rcu_idle);
                drop(irq_guard);
            } else {
                error!("Idle process should not be scheduled with IRQs disabled.");
                spin_loop();
            }
        }
    }
}
