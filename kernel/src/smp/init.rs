use log::info;

use crate::{
    arch::{syscall::arch_syscall_init, CurrentIrqArch, CurrentSchedArch},
    exception::InterruptArch,
    process::ProcessManager,
    sched::SchedArch,
    smp::{core::smp_get_processor_id, cpu::smp_cpu_manager},
};

#[inline(never)]
pub fn smp_ap_start_stage2() -> ! {
    assert!(!CurrentIrqArch::is_irq_enabled());

    smp_cpu_manager().complete_ap_thread(true);

    do_ap_start_stage2();

    CurrentSchedArch::initial_setup_sched_local();

    CurrentSchedArch::enable_sched_local();
    ProcessManager::arch_idle_func();
}

#[inline(never)]
fn do_ap_start_stage2() {
    info!("Successfully started AP {}", smp_get_processor_id().data());
    arch_syscall_init().expect("AP core failed to initialize syscall");
}
