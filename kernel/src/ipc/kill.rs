use crate::arch::ipc::signal::{SigCode, Signal};
use crate::ipc::signal_types::{SigInfo, SigType};
use crate::process::{process_group::Pgid, Pid, ProcessManager};
use core::sync::atomic::compiler_fence;
use system_error::SystemError;

/// ### 杀死一个进程
pub fn kill_process(pid: Pid, sig: Signal) -> Result<usize, SystemError> {
    // 初始化signal info
    let mut info = SigInfo::new(sig, 0, SigCode::User, SigType::Kill(pid));
    compiler_fence(core::sync::atomic::Ordering::SeqCst);

    let ret = sig
        .send_signal_info(Some(&mut info), pid)
        .map(|x| x as usize);

    compiler_fence(core::sync::atomic::Ordering::SeqCst);
    ret
}

/// ### 杀死一个进程组
pub fn kill_process_group(pgid: Pgid, sig: Signal) -> Result<usize, SystemError> {
    let pg = ProcessManager::find_process_group(pgid).ok_or(SystemError::ESRCH)?;
    let inner = pg.process_group_inner.lock();
    for pcb in inner.processes.values() {
        kill_process(pcb.pid(), sig)?; // Call the new common function
    }
    Ok(0)
}

/// ### 杀死所有进程
/// - 该函数会杀死所有进程，除了当前进程和init进程
pub fn kill_all(sig: Signal) -> Result<usize, SystemError> {
    let current_pid = ProcessManager::current_pcb().pid();
    let all_processes = ProcessManager::get_all_processes();

    for pid_val in all_processes {
        if pid_val == current_pid || pid_val.data() == 1 {
            continue;
        }
        kill_process(pid_val, sig)?; // Call the new common function
    }
    Ok(0)
}
