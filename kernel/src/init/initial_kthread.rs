//! 这个文件内放置初始内核线程的代码。

use core::sync::atomic::{compiler_fence, Ordering};

use alloc::string::{String, ToString};
use system_error::SystemError;

use crate::{
    arch::{interrupt::TrapFrame, process::arch_switch_to_user},
    driver::{net::e1000e::e1000e::e1000e_init, virtio::virtio::virtio_probe},
    filesystem::vfs::core::mount_root_fs,
    kdebug, kerror,
    net::net_core::net_init,
    process::{kthread::KernelThreadMechanism, stdio::stdio_init, ProcessFlags, ProcessManager},
    smp::smp_init,
    syscall::Syscall,
};

use super::initcall::do_initcalls;

pub fn initial_kernel_thread() -> i32 {
    kernel_init().unwrap_or_else(|err| {
        panic!("Failed to initialize kernel: {:?}", err);
    });

    switch_to_user();

    unreachable!();
}

fn kernel_init() -> Result<(), SystemError> {
    KernelThreadMechanism::init_stage2();
    kenrel_init_freeable()?;

    // 由于目前加锁，速度过慢，所以先不开启双缓冲
    // scm_enable_double_buffer().expect("Failed to enable double buffer");

    #[cfg(target_arch = "x86_64")]
    crate::driver::disk::ahci::ahci_init().expect("Failed to initialize AHCI");

    virtio_probe();
    mount_root_fs().expect("Failed to mount root fs");

    e1000e_init();
    net_init().unwrap_or_else(|err| {
        kerror!("Failed to initialize network: {:?}", err);
    });

    kdebug!("initial kernel thread done.");

    return Ok(());
}

#[inline(never)]
fn kenrel_init_freeable() -> Result<(), SystemError> {
    do_initcalls().unwrap_or_else(|err| {
        panic!("Failed to initialize subsystems: {:?}", err);
    });
    stdio_init().expect("Failed to initialize stdio");
    smp_init();

    return Ok(());
}

/// 切换到用户态
fn switch_to_user() {
    let current_pcb = ProcessManager::current_pcb();

    // 删除kthread的标志
    current_pcb.flags().remove(ProcessFlags::KTHREAD);
    current_pcb.worker_private().take();

    *current_pcb.sched_info().sched_policy.write_irqsave() = crate::sched::SchedPolicy::CFS;
    drop(current_pcb);

    // 逐个尝试运行init进程

    if try_to_run_init_process("/bin/dragonreach").is_err()
        && try_to_run_init_process("/bin/init").is_err()
        && try_to_run_init_process("/bin/sh").is_err()
    {
        panic!("Failed to run init process: No working init found.");
    }
}

fn try_to_run_init_process(path: &str) -> Result<(), SystemError> {
    if let Err(e) = run_init_process(path.to_string()) {
        if e != SystemError::ENOENT {
            kerror!(
                "Failed to run init process: {path} exists but couldn't execute it (error {:?})",
                e
            );
        }
        return Err(e);
    }

    unreachable!();
}

#[inline(never)]
fn run_init_process(path: String) -> Result<(), SystemError> {
    let argv = vec![path.clone()];
    let envp = vec![String::from("PATH=/")];
    let mut trap_frame = TrapFrame::new();

    compiler_fence(Ordering::SeqCst);
    Syscall::do_execve(path, argv, envp, &mut trap_frame)?;

    // 需要确保执行到这里之后，上面所有的资源都已经释放（比如arc之类的）

    unsafe { arch_switch_to_user(trap_frame) };
}
