//! 这个文件内放置初始内核线程的代码。

use core::sync::atomic::{compiler_fence, Ordering};

use alloc::{ffi::CString, string::ToString};
use log::{debug, error};
use system_error::SystemError;

use crate::{
    arch::{interrupt::TrapFrame, process::arch_switch_to_user},
    driver::{net::e1000e::e1000e::e1000e_init, virtio::virtio::virtio_probe},
    filesystem::vfs::core::mount_root_fs,
    net::net_core::net_init,
    process::{
        exec::ProcInitInfo, kthread::KernelThreadMechanism, stdio::stdio_init, ProcessFlags,
        ProcessManager,
    },
    smp::smp_init,
    syscall::Syscall,
};

use super::{cmdline::kenrel_cmdline_param_manager, initcall::do_initcalls};

const INIT_PROC_TRYLIST: [&str; 3] = ["/bin/dragonreach", "/bin/init", "/bin/sh"];

pub fn initial_kernel_thread() -> i32 {
    kernel_init().unwrap_or_else(|err| {
        log::error!("Failed to initialize kernel: {:?}", err);
        panic!()
    });

    switch_to_user();
}

fn kernel_init() -> Result<(), SystemError> {
    KernelThreadMechanism::init_stage2();
    kenrel_init_freeable()?;
    #[cfg(target_arch = "x86_64")]
    crate::driver::disk::ahci::ahci_init()
        .inspect_err(|e| log::error!("ahci_init failed: {:?}", e))
        .ok();
    virtio_probe();
    mount_root_fs().expect("Failed to mount root fs");
    e1000e_init();
    net_init().unwrap_or_else(|err| {
        error!("Failed to initialize network: {:?}", err);
    });

    debug!("initial kernel thread done.");

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
#[inline(never)]
fn switch_to_user() -> ! {
    let current_pcb = ProcessManager::current_pcb();

    // 删除kthread的标志
    current_pcb.flags().remove(ProcessFlags::KTHREAD);
    current_pcb.worker_private().take();

    *current_pcb.sched_info().sched_policy.write_irqsave() = crate::sched::SchedPolicy::CFS;
    drop(current_pcb);

    let mut proc_init_info = ProcInitInfo::new("");
    proc_init_info.envs.push(CString::new("PATH=/").unwrap());
    proc_init_info.args = kenrel_cmdline_param_manager().init_proc_args();
    proc_init_info.envs = kenrel_cmdline_param_manager().init_proc_envs();

    let mut trap_frame = TrapFrame::new();

    if let Some(path) = kenrel_cmdline_param_manager().init_proc_path() {
        log::info!("Boot with specified init process: {:?}", path);

        try_to_run_init_process(
            path.as_c_str().to_str().unwrap(),
            &mut proc_init_info,
            &mut trap_frame,
        )
        .expect(format!("Failed to run specified init process: {:?}", path).as_str());
    } else {
        let mut ok = false;
        for path in INIT_PROC_TRYLIST.iter() {
            if try_to_run_init_process(path, &mut proc_init_info, &mut trap_frame).is_ok() {
                ok = true;
                break;
            }
        }
        if !ok {
            panic!("Failed to run init process: No working init found.");
        }
    }
    drop(proc_init_info);
    // 需要确保执行到这里之后，上面所有的资源都已经释放（比如arc之类的）
    compiler_fence(Ordering::SeqCst);

    unsafe { arch_switch_to_user(trap_frame) };
}

fn try_to_run_init_process(
    path: &str,
    proc_init_info: &mut ProcInitInfo,
    trap_frame: &mut TrapFrame,
) -> Result<(), SystemError> {
    proc_init_info.proc_name = CString::new(path).unwrap();
    proc_init_info.args.insert(0, CString::new(path).unwrap());
    if let Err(e) = run_init_process(&proc_init_info, trap_frame) {
        if e != SystemError::ENOENT {
            error!(
                "Failed to run init process: {path} exists but couldn't execute it (error {:?})",
                e
            );
        }

        proc_init_info.args.remove(0);
        return Err(e);
    }
    Ok(())
}

fn run_init_process(
    proc_init_info: &ProcInitInfo,
    trap_frame: &mut TrapFrame,
) -> Result<(), SystemError> {
    compiler_fence(Ordering::SeqCst);
    let path = proc_init_info.proc_name.to_str().unwrap();

    debug!("Init proc arguments:");

    for arg in &proc_init_info.args {
        debug!("arg: {:?}", arg);
    }
    debug!("Init proc environments:");
    for env in &proc_init_info.envs {
        debug!("env: {:?}", env);
    }

    Syscall::do_execve(
        path.to_string(),
        proc_init_info.args.clone(),
        proc_init_info.envs.clone(),
        trap_frame,
    )?;
    Ok(())
}
