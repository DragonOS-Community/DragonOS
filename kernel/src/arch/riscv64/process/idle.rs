use core::hint::spin_loop;

use log::error;

use crate::{arch::CurrentIrqArch, exception::InterruptArch, process::ProcessManager};

impl ProcessManager {
    /// 每个核的idle进程
    pub fn arch_idle_func() -> ! {
        loop {
            if CurrentIrqArch::is_irq_enabled() {
                let rcu_idle = crate::rcu::idle_enter();
                riscv::asm::wfi();
                crate::rcu::idle_exit(rcu_idle);
            } else {
                error!("Idle process should not be scheduled with IRQs disabled.");
                spin_loop();
            }

            // debug!("idle loop");
        }
    }
}
