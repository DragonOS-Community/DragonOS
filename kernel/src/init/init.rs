use crate::{
    arch::{
        init::{early_setup_arch, setup_arch, setup_arch_post},
        CurrentIrqArch, CurrentSMPArch, CurrentSchedArch,
    },
    driver::{base::init::driver_init, serial::serial_early_init, video::VideoRefreshManager},
    exception::{init::irq_init, softirq::softirq_init, InterruptArch},
    filesystem::vfs::core::vfs_init,
    include::bindings::bindings::acpi_init,
    init::init_intertrait,
    libs::{
        futex::futex::Futex,
        lib_ui::{
            screen_manager::{scm_init, scm_reinit},
            textui::textui_init,
        },
    },
    mm::init::mm_init,
    process::{kthread::kthread_init, process_init, ProcessManager},
    sched::{core::sched_init, SchedArch},
    smp::{early_smp_init, SMPArch},
    syscall::Syscall,
    time::{
        clocksource::clocksource_boot_finish, timekeeping::timekeeping_init, timer::timer_init,
    },
};

/// The entry point for the kernel
///
/// 前面可能会有一个架构相关的函数
pub fn start_kernel() -> ! {
    // 进入内核后，中断应该是关闭的
    assert_eq!(CurrentIrqArch::is_irq_enabled(), false);

    do_start_kernel();

    CurrentSchedArch::initial_setup_sched_local();

    CurrentSchedArch::enable_sched_local();

    ProcessManager::arch_idle_func();
}

#[inline(never)]
fn do_start_kernel() {
    init_before_mem_init();

    early_setup_arch().expect("setup_arch failed");
    unsafe { mm_init() };

    scm_reinit().unwrap();
    textui_init().unwrap();
    init_intertrait();

    vfs_init().expect("vfs init failed");
    driver_init().expect("driver init failed");

    #[cfg(target_arch = "x86_64")]
    unsafe {
        acpi_init()
    };

    early_smp_init().expect("early smp init failed");
    irq_init().expect("irq init failed");
    setup_arch().expect("setup_arch failed");
    CurrentSMPArch::prepare_cpus().expect("prepare_cpus failed");

    process_init();
    sched_init();
    softirq_init().expect("softirq init failed");
    Syscall::init().expect("syscall init failed");
    timekeeping_init();
    timer_init();
    kthread_init();
    clocksource_boot_finish();

    CurrentSMPArch::init().expect("smp init failed");
    // SMP初始化有可能会开中断，所以这里再次检查中断是否关闭
    assert_eq!(CurrentIrqArch::is_irq_enabled(), false);
    Futex::init();

    setup_arch_post().expect("setup_arch_post failed");

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
