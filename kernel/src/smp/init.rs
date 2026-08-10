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

    if let Err(error) = do_ap_start_stage2() {
        smp_cpu_manager().complete_ap_thread(true, Err(error));
        park_failed_ap();
    }

    CurrentSchedArch::initial_setup_sched_local();
    CurrentSchedArch::enable_sched_local();

    // Publish Online only after every mandatory per-CPU facility, including
    // the local scheduler, is ready to accept remote enqueue/wakeup work.
    smp_cpu_manager().complete_ap_thread(true, Ok(()));
    ProcessManager::arch_idle_func();
}

/// Keep a failed AP offline without consuming a host CPU. Interrupts are
/// still disabled here, so these instructions form a terminal park until a
/// future hotplug protocol explicitly resets the CPU.
fn park_failed_ap() -> ! {
    #[cfg(target_arch = "x86_64")]
    loop {
        unsafe { x86::halt() };
    }

    #[cfg(target_arch = "riscv64")]
    loop {
        riscv::asm::wfi();
    }

    #[cfg(target_arch = "loongarch64")]
    loop {
        core::hint::spin_loop();
    }
}

#[inline(never)]
fn do_ap_start_stage2() -> Result<(), system_error::SystemError> {
    info!("Successfully started AP {}", smp_get_processor_id().data());
    #[cfg(target_arch = "x86_64")]
    {
        crate::arch::x86_64::mm::X86_64MMArch::init_current_cpu_nxe();
        crate::driver::clocksource::kvm_clock::kvmclock_init_secondary()?;
    }
    arch_syscall_init()?;
    Ok(())
}
