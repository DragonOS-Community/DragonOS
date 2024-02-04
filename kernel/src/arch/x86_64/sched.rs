use crate::{
    exception::InterruptArch, include::bindings::bindings::enter_syscall_int, sched::SchedArch,
    syscall::SYS_SCHED,
};

use super::CurrentIrqArch;

/// @brief 若内核代码不处在中断上下文中，那么将可以使用本函数，发起一个sys_sched系统调用，然后运行调度器。
/// 由于只能在中断上下文中进行进程切换，因此需要发起一个系统调用SYS_SCHED。
#[no_mangle]
pub extern "C" fn sched() {
    unsafe {
        enter_syscall_int(SYS_SCHED as u64, 0, 0, 0, 0, 0, 0, 0, 0);
    }
}

extern "C" {
    fn apic_timer_init();
}

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
        unsafe {
            apic_timer_init();
        }
    }
}
