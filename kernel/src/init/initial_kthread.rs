//! 这个文件内放置初始内核线程的代码。

use core::sync::atomic::{compiler_fence, Ordering};

use alloc::ffi::CString;
use log::{debug, error};
use system_error::SystemError;

#[cfg(feature = "initram")]
use crate::filesystem::vfs::vcore::change_root_fs;

use crate::{
    arch::{interrupt::TrapFrame, process::arch_switch_to_user},
    driver::net::e1000e::e1000e::e1000e_init,
    filesystem::vfs::vcore::mount_root_fs,
    net::net_core::net_init,
    process::{
        exec::ProcInitInfo, execve::do_execve, kthread::KernelThreadMechanism, stdio::stdio_init,
        ProcessFlags, ProcessManager,
    },
    rcu,
    smp::smp_init,
};

use super::{cmdline::kenrel_cmdline_param_manager, initcall::do_initcalls};

const INIT_PROC_TRYLIST: [(&str, Option<&str>); 5] = [
    ("/bin/busybox", Some("init")),
    ("/bin/init", None),
    ("/sbin/init", None),
    ("/bin/sh", None),
    ("/bin/riscv_rust_init", None), // 对vf2, 目前没做cmdline的适配, 加个默认寻找
];

static mut SYSTEM_STATE: SystemState = SystemState::Booting;

#[allow(dead_code)]
pub fn get_system_state() -> SystemState {
    unsafe { SYSTEM_STATE }
}

pub fn set_system_state(state: SystemState) {
    unsafe {
        SYSTEM_STATE = state;
    }
}

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SystemState {
    Booting,
    Scheduling,
    FreeingInitMem,
    Running,
    Halt,
    PowerOff,
    Restart,
    Suspend,
}

pub fn initial_kernel_thread() -> i32 {
    kernel_init().unwrap_or_else(|err| {
        log::error!("Failed to initialize kernel: {:?}", err);
        panic!()
    });

    switch_to_user();
}

fn kernel_init() -> Result<(), SystemError> {
    KernelThreadMechanism::init_stage2();
    rcu::start_worker();
    crate::misc::reboot::reboot_notifier_init()?;
    kenrel_init_freeable()?;
    set_system_state(SystemState::FreeingInitMem);
    if super::initramfs_enabled() {
        // 使用 initramfs, 迁移文件系统
        #[cfg(feature = "initram")]
        change_root_fs().expect("Failed to mount root fs");
    } else {
        // 不使用 initramfs, 正常启动
        mount_root_fs().expect("Failed to mount root fs");
    }

    // WARNING: We must keep `mount_root_fs` before stdio_init,
    // because `migrate_virtual_filesystem` will change the root directory of the file system.
    stdio_init().expect("Failed to initialize stdio");

    e1000e_init();
    net_init().unwrap_or_else(|err| {
        error!("Failed to initialize network: {:?}", err);
    });

    #[cfg(feature = "fifo_demo")]
    crate::sched::fifo_demo::fifo_demo_init();

    debug!("initial kernel thread done.");
    set_system_state(SystemState::Running);

    return Ok(());
}

#[inline(never)]
fn kenrel_init_freeable() -> Result<(), SystemError> {
    do_initcalls().unwrap_or_else(|err| {
        panic!("Failed to initialize subsystems: {:?}", err);
    });
    smp_init();
    #[cfg(target_arch = "x86_64")]
    {
        if let Err(error) = crate::text_patch::init_live() {
            log::warn!("runtime text patching unavailable: {:?}", error);
        } else if let Err(error) = crate::debug::jump_label::static_keys_live_selftest() {
            panic!("static-key live selftest failed: {:?}", error);
        }
    }
    #[cfg(not(target_arch = "x86_64"))]
    let _ = crate::text_patch::init_live();
    crate::exception::workqueue::workqueue_init();
    crate::perf::release::init();
    return Ok(());
}

/// 切换到用户态
///
/// 将 init 内核线程切换到用户态。
#[inline(never)]
fn switch_to_user() -> ! {
    let current_pcb = ProcessManager::current_pcb();

    // 删除kthread的标志
    current_pcb.flags().remove(ProcessFlags::KTHREAD);
    current_pcb.worker_private().take();

    drop(current_pcb);

    let mut proc_init_info = ProcInitInfo::new("");
    // 设置默认环境变量
    set_init_env(&mut proc_init_info.envs, CString::new("HOME=/").unwrap());
    set_init_env(
        &mut proc_init_info.envs,
        CString::new("TERM=linux").unwrap(),
    );
    proc_init_info.args = kenrel_cmdline_param_manager().init_proc_args();
    // 命令行管理器提供的环境变量会追加到默认环境变量之后
    for env in kenrel_cmdline_param_manager().init_proc_envs() {
        set_init_env(&mut proc_init_info.envs, env);
    }

    let mut trap_frame = TrapFrame::new();

    if super::initramfs_enabled() {
        // 使用 initramfs, 启动 /init
        log::info!("Initramfs, Boot with specified init process: /init");

        try_to_run_init_process("/init", &mut proc_init_info, &None, &mut trap_frame)
            .unwrap_or_else(|e| {
                panic!("Failed to run specified init process: /init, err: {:?}", e)
            });
    } else if let Some(path) = kenrel_cmdline_param_manager().init_proc_path() {
        log::info!("Boot with specified init process: {:?}", path);

        try_to_run_init_process(
            path.as_c_str().to_str().unwrap(),
            &mut proc_init_info,
            &None,
            &mut trap_frame,
        )
        .unwrap_or_else(|e| {
            panic!(
                "Failed to run specified init process: {:?}, err: {:?}",
                path, e
            )
        });
    } else {
        let mut ok = false;
        for (path, ext_args) in INIT_PROC_TRYLIST.iter() {
            if try_to_run_init_process(path, &mut proc_init_info, ext_args, &mut trap_frame).is_ok()
            {
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
    ext_args: &Option<&str>,
    trap_frame: &mut TrapFrame,
) -> Result<(), SystemError> {
    log::debug!("Trying to run init process at {:?}", path);
    let mut args_to_insert = alloc::vec::Vec::new();
    args_to_insert.push(CString::new(path).unwrap());

    if let Some(ext_args) = ext_args {
        // Split ext_args by whitespace and trim each part
        for arg in ext_args.split_whitespace() {
            args_to_insert.push(CString::new(arg.trim()).unwrap());
        }
    }
    proc_init_info.proc_name = CString::new(path).unwrap();
    let elements_to_remove = args_to_insert.len();
    let old_args = core::mem::replace(&mut proc_init_info.args, args_to_insert);
    proc_init_info.args.extend(old_args);
    if let Err(e) = run_init_process(proc_init_info, trap_frame) {
        if e != SystemError::ENOENT {
            error!(
                "Failed to run init process: {path} exists but couldn't execute it (error {:?})",
                e
            );
        }

        proc_init_info.args.drain(0..elements_to_remove);
        return Err(e);
    }

    Ok(())
}

fn set_init_env(envs: &mut alloc::vec::Vec<CString>, env: CString) {
    let key_len = env
        .as_bytes()
        .iter()
        .position(|&byte| byte == b'=')
        .unwrap_or(env.as_bytes().len());
    if let Some(slot) = envs
        .iter_mut()
        .find(|candidate| init_env_key_matches(candidate, &env.as_bytes()[..key_len]))
    {
        *slot = env;
    } else {
        envs.push(env);
    }
}

fn init_env_key_matches(env: &CString, key: &[u8]) -> bool {
    let bytes = env.as_bytes();
    bytes.len() > key.len() && bytes.starts_with(key) && bytes[key.len()] == b'='
}

fn run_init_process(
    proc_init_info: &ProcInitInfo,
    trap_frame: &mut TrapFrame,
) -> Result<(), SystemError> {
    compiler_fence(Ordering::SeqCst);
    let path = proc_init_info.proc_name.to_str().unwrap();

    do_execve(
        path,
        proc_init_info.args.clone(),
        proc_init_info.envs.clone(),
        trap_frame,
    )?;
    // 初始化阶段直接调用 do_execve（绕过 sys_execve），因此这里补齐 cmdline 存储
    ProcessManager::current_pcb().set_cmdline_from_argv(&proc_init_info.args);
    Ok(())
}
