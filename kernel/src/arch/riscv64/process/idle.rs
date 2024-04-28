use core::hint::spin_loop;

use crate::{arch::CurrentIrqArch, exception::InterruptArch, kBUG, process::ProcessManager};

impl ProcessManager {
    /// 每个核的idle进程
    pub fn arch_idle_func() -> ! {
        loop {
            if CurrentIrqArch::is_irq_enabled() {
                riscv::asm::wfi();
            } else {
                kBUG!("Idle process should not be scheduled with IRQs disabled.");
                spin_loop();
            }

            // kdebug!("idle loop");
        }
    }
}
