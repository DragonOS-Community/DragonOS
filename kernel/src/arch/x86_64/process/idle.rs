use core::hint::spin_loop;

use crate::{
    arch::CurrentIrqArch,
    exception::InterruptArch,
    kBUG, kinfo,
    new_sched::{SchedMode, __schedule},
    process::{ProcessFlags, ProcessManager},
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
                unsafe {
                    x86::halt();
                }
            } else {
                kBUG!("Idle process should not be scheduled with IRQs disabled.");
                spin_loop();
            }
        }
    }
}
