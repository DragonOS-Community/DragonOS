use crate::ipc::signal_types::{SigInfo, SigType};
use crate::process::pid::{Pid, PidType};
use crate::process::{ProcessControlBlock, ProcessManager, RawPid};
use crate::{arch::ipc::signal::Signal, ipc::signal_types::SigCode};
use alloc::sync::Arc;
use core::sync::atomic::compiler_fence;
use system_error::SystemError;

/// ### 杀死一个进程
pub fn kill_process(pid: RawPid, sig: Signal) -> Result<usize, SystemError> {
    // 初始化signal info
    let mut info = SigInfo::new(sig, 0, SigCode::User, SigType::Kill(pid));
    compiler_fence(core::sync::atomic::Ordering::SeqCst);

    let ret = sig
        .send_signal_info(Some(&mut info), pid)
        .map(|x| x as usize);

    compiler_fence(core::sync::atomic::Ordering::SeqCst);
    ret
}

/// 直接向指定进程发送信号，绕过PID namespace查找
///
/// 注意！这个函数不会检查目标进程是否在本pidns内，慎用！可能造成安全问题。
pub fn kill_process_by_pcb(
    pcb: Arc<ProcessControlBlock>,
    sig: Signal,
) -> Result<usize, SystemError> {
    // 初始化signal info
    let mut info = SigInfo::new(sig, 0, SigCode::User, SigType::Kill(pcb.raw_pid()));

    return sig
        .send_signal_info_to_pcb(Some(&mut info), pcb)
        .map(|x| x as usize);
}
/// ### 杀死一个进程组
///
/// 注意！这个函数的实现跟Linux不一致。
///
/// 参考 https://code.dragonos.org.cn/xref/linux-6.6.21/kernel/signal.c?fi=kill_pgrp#1921
#[inline(never)]
pub fn kill_process_group(pgid: &Arc<Pid>, sig: Signal) -> Result<usize, SystemError> {
    for pcb in pgid.tasks_iter(PidType::PGID) {
        kill_process(pcb.raw_pid(), sig)?;
    }
    Ok(0)
}

/// ### 杀死所有进程
/// - 该函数会杀死所有进程，除了当前进程和init进程
pub fn kill_all(sig: Signal) -> Result<usize, SystemError> {
    let current_pid = ProcessManager::current_pcb().raw_pid();
    let all_processes = ProcessManager::get_all_processes();

    for pid_val in all_processes {
        if pid_val == current_pid || pid_val.data() == 1 {
            continue;
        }
        kill_process(pid_val, sig)?; // Call the new common function
    }
    Ok(0)
}
