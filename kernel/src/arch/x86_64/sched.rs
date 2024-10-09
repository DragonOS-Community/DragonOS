use core::hint::spin_loop;

use crate::{exception::InterruptArch, sched::SchedArch, smp::core::smp_get_processor_id};

use super::{driver::apic::apic_timer::apic_timer_init, CurrentIrqArch};

static mut BSP_INIT_OK: bool = false;

pub struct X86_64SchedArch;

impl SchedArch for X86_64SchedArch {
    fn enable_sched_local() {
        // fixme: 这里将来可能需要更改，毕竟这个直接开关中断有点暴力。
        unsafe { CurrentIrqArch::interrupt_enable() };
    }

    fn disable_sched_local() {
        unsafe {
            CurrentIrqArch::interrupt_disable();
        }
    }

    fn initial_setup_sched_local() {
        let irq_guard = unsafe { CurrentIrqArch::save_and_disable_irq() };

        let cpu_id = smp_get_processor_id();

        if cpu_id.data() != 0 {
            while !unsafe { BSP_INIT_OK } {
                spin_loop();
            }
        }

        apic_timer_init();
        if smp_get_processor_id().data() == 0 {
            unsafe {
                BSP_INIT_OK = true;
            }
        }

        drop(irq_guard);
    }
}
