use crate::{
    arch::{
        init::{early_setup_arch, setup_arch, setup_arch_post},
        time::time_init,
        CurrentIrqArch, CurrentSMPArch, CurrentSchedArch,
    },
    driver::{
        acpi::acpi_init, base::init::driver_init, serial::serial_early_init,
        video::VideoRefreshManager,
    },
    exception::{init::irq_init, softirq::softirq_init, InterruptArch},
    filesystem::vfs::core::vfs_init,
    init::init_intertrait,
    libs::{
        futex::futex::Futex,
        lib_ui::{
            screen_manager::{scm_init, scm_reinit},
            textui::textui_init,
        },
        printk::early_init_logging,
    },
    mm::init::mm_init,
    process::{kthread::kthread_init, process_init, ProcessManager},
    sched::SchedArch,
    smp::{early_smp_init, SMPArch},
    syscall::Syscall,
    time::{
        clocksource::clocksource_boot_finish, timekeeping::timekeeping_init, timer::timer_init,
    },
};

use super::boot::boot_callback_except_early;

/// The entry point for the kernel
///
/// 前面可能会有一个架构相关的函数
pub fn start_kernel() -> ! {
    // 进入内核后，中断应该是关闭的
    assert!(!CurrentIrqArch::is_irq_enabled());

    do_start_kernel();

    CurrentSchedArch::initial_setup_sched_local();

    CurrentSchedArch::enable_sched_local();

    ProcessManager::arch_idle_func();
}

#[inline(never)]
fn do_start_kernel() {
    init_before_mem_init();
    early_init_logging();

    early_setup_arch().expect("setup_arch failed");
    unsafe { mm_init() };

    scm_reinit().unwrap();
    textui_init().unwrap();

    boot_callback_except_early();
    init_intertrait();

    vfs_init().expect("vfs init failed");
    driver_init().expect("driver init failed");

    acpi_init().expect("acpi init failed");
    crate::sched::sched_init();
    process_init();
    early_smp_init().expect("early smp init failed");
    irq_init().expect("irq init failed");
    setup_arch().expect("setup_arch failed");
    CurrentSMPArch::prepare_cpus().expect("prepare_cpus failed");

    // sched_init();
    softirq_init().expect("softirq init failed");
    Syscall::init().expect("syscall init failed");
    timekeeping_init();
    time_init();
    timer_init();
    kthread_init();
    setup_arch_post().expect("setup_arch_post failed");
    clocksource_boot_finish();

    Futex::init();

    #[cfg(all(target_arch = "x86_64", feature = "kvm"))]
    crate::virt::kvm::kvm_init();
}

/// 在内存管理初始化之前，执行的初始化
#[inline(never)]
fn init_before_mem_init() {
    serial_early_init().expect("serial early init failed");
    let video_ok = unsafe { VideoRefreshManager::video_init().is_ok() };
    scm_init(video_ok);
}
