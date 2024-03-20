use core::hint::spin_loop;

use crate::{
    exception::InterruptArch, include::bindings::bindings::enter_syscall_int, sched::SchedArch,
    smp::core::smp_get_processor_id, syscall::SYS_SCHED,
};

use super::{driver::apic::apic_timer::apic_timer_init, CurrentIrqArch};

/// @brief 若内核代码不处在中断上下文中，那么将可以使用本函数，发起一个sys_sched系统调用，然后运行调度器。
/// 由于只能在中断上下文中进行进程切换，因此需要发起一个系统调用SYS_SCHED。
#[no_mangle]
pub extern "C" fn sched() {
    unsafe {
        enter_syscall_int(SYS_SCHED as u64, 0, 0, 0, 0, 0, 0);
    }
}

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
