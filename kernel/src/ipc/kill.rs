use crate::{
    arch::ipc::signal::Signal,
    ipc::{
        signal_types::{OriginCode, SigCode, SigInfo, SigType},
        syscall::sys_kill::check_signal_permission_pcb_with_sig,
    },
    process::{
        pid::{Pid, PidType},
        ProcessControlBlock, ProcessManager, RawPid,
    },
};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::compiler_fence;
use system_error::SystemError;

/// ### 向一个进程发送信号
/// kill() 系统调用发送进程级信号，使用 PidType::TGID
pub fn send_signal_to_pid(pid: RawPid, sig: Signal) -> Result<usize, SystemError> {
    // 查找目标进程
    let target = ProcessManager::find_task_by_vpid(pid).ok_or(SystemError::ESRCH)?;

    // 检查权限（传入信号以处理 SIGCONT 特殊情况）
    check_signal_permission_pcb_with_sig(&target, Some(sig))?;

    // 初始化signal info
    let current_pcb = ProcessManager::current_pcb();
    let sender_pid = current_pcb.raw_pid();
    let sender_uid = current_pcb.cred().uid.data() as u32;
    let mut info = SigInfo::new(
        sig,
        0,
        SigCode::Origin(OriginCode::User),
        SigType::Kill {
            pid: sender_pid,
            uid: sender_uid,
        },
    );
    compiler_fence(core::sync::atomic::Ordering::SeqCst);

    // kill() 发送进程级信号，使用 PidType::TGID
    let ret = sig
        .send_signal_info_to_pcb(Some(&mut info), target, PidType::TGID)
        .map(|x| x as usize);

    compiler_fence(core::sync::atomic::Ordering::SeqCst);
    ret
}

/// 直接向指定进程发送信号，绕过PID namespace查找
///
/// 注意！这个函数不会检查目标进程是否在本pidns内，慎用！可能造成安全问题。
pub fn send_signal_to_pcb(
    pcb: Arc<ProcessControlBlock>,
    sig: Signal,
) -> Result<usize, SystemError> {
    // 初始化signal info
    let current_pcb = ProcessManager::current_pcb();
    let sender_pid = current_pcb.raw_pid();
    let sender_uid = current_pcb.cred().uid.data() as u32;
    let mut info = SigInfo::new(
        sig,
        0,
        SigCode::Origin(OriginCode::User),
        SigType::Kill {
            pid: sender_pid,
            uid: sender_uid,
        },
    );

    // 发送进程级信号，使用 PidType::TGID
    return sig
        .send_signal_info_to_pcb(Some(&mut info), pcb, PidType::TGID)
        .map(|x| x as usize);
}

/// ### 向一个进程组发送信号
///
/// 参考 https://code.dragonos.org.cn/xref/linux-6.6.21/kernel/signal.c?fi=kill_pgrp#1921
#[inline(never)]
pub fn send_signal_to_pgid(pgid: &Arc<Pid>, sig: Signal) -> Result<usize, SystemError> {
    // 先收集进程组中的所有进程，避免在持有锁时调用复杂操作
    let tasks: Vec<Arc<ProcessControlBlock>> = pgid.tasks_iter(PidType::PGID).collect();

    // 如果进程组中没有任何进程，返回 ESRCH
    if tasks.is_empty() {
        return Err(SystemError::ESRCH);
    }

    let mut success = false;
    let mut last_err = None;

    for pcb in tasks {
        // 检查权限（传入信号以处理 SIGCONT 特殊情况）
        if let Err(e) = check_signal_permission_pcb_with_sig(&pcb, Some(sig)) {
            if !success {
                last_err = Some(e);
            }
            continue;
        }

        // 初始化signal info
        let current_pcb = ProcessManager::current_pcb();
        let sender_pid = current_pcb.raw_pid();
        let sender_uid = current_pcb.cred().uid.data() as u32;
        let mut info = SigInfo::new(
            sig,
            0,
            SigCode::Origin(OriginCode::User),
            SigType::Kill {
                pid: sender_pid,
                uid: sender_uid,
            },
        );

        // 进程组信号使用 PidType::TGID
        if let Err(e) = sig.send_signal_info_to_pcb(Some(&mut info), pcb, PidType::TGID) {
            if !success {
                last_err = Some(e);
            }
        } else {
            // 至少有一个成功
            success = true;
        }
    }

    // 只要有一个成功，就返回成功
    if success {
        return Ok(0);
    }

    // 所有进程都失败，返回最后的错误
    Err(last_err.unwrap_or(SystemError::ESRCH))
}

/// ### 向所有进程发送信号
/// - 该函数会向所有进程发送信号，除了当前进程和init进程
pub fn send_signal_to_all(sig: Signal) -> Result<usize, SystemError> {
    let current_pid = ProcessManager::current_pcb().raw_pid();
    let all_processes = ProcessManager::get_all_processes();

    for pid_val in all_processes {
        if pid_val == current_pid || pid_val.data() == 1 {
            continue;
        }
        send_signal_to_pid(pid_val, sig)?; // Call the new common function
    }
    Ok(0)
}
