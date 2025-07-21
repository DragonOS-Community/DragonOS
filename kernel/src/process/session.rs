use super::{pid::Pid, ProcessControlBlock, ProcessManager, RawPid};
use crate::{driver::tty::tty_job_control::TtyJobCtrlManager, process::pid::PidType};
use alloc::sync::Arc;
use system_error::SystemError;

/// 参考 https://code.dragonos.org.cn/xref/linux-6.6.21/kernel/sys.c#1225
pub(super) fn ksys_setsid() -> Result<RawPid, SystemError> {
    let pcb = ProcessManager::current_pcb();
    let group_leader = pcb
        .threads_read_irqsave()
        .group_leader()
        .ok_or(SystemError::ESRCH)?;
    let sid = group_leader.pid();
    let session = sid.pid_vnr();
    log::debug!(
        "ksys_setsid: group_leader: {}",
        group_leader.raw_pid().data()
    );
    let siginfo_lock = group_leader.sig_info_upgradable();
    // Fail if pcb already a session leader
    if siginfo_lock.is_session_leader {
        return Err(SystemError::EPERM);
    }

    // Fail if a process group id already exists that equals the
    // proposed session id.
    if sid.pid_task(PidType::PGID).is_some() {
        return Err(SystemError::EPERM);
    }

    let mut siginfo_guard = siginfo_lock.upgrade();
    siginfo_guard.is_session_leader = true;
    set_special_pids(&group_leader, &sid);

    TtyJobCtrlManager::__proc_clear_tty(&mut siginfo_guard);
    return Ok(session);
}

fn set_special_pids(current_session_group_leader: &Arc<ProcessControlBlock>, sid: &Arc<Pid>) {
    let session = current_session_group_leader.task_session();
    let change_sid = match session {
        Some(s) => !Arc::ptr_eq(&s, sid),
        None => true,
    };

    let pgrp = current_session_group_leader.task_pgrp();
    let change_pgrp = match pgrp {
        Some(pg) => !Arc::ptr_eq(&pg, sid),
        None => true,
    };
    log::debug!(
        "leader: {}, change sid: {}, pgrp: {}, sid_raw: {}",
        current_session_group_leader.raw_pid().data(),
        change_sid,
        change_pgrp,
        sid.pid_vnr().data()
    );
    if change_sid {
        current_session_group_leader.change_pid(PidType::SID, sid.clone());
    }
    if change_pgrp {
        current_session_group_leader.change_pid(PidType::PGID, sid.clone());
    }

    log::debug!(
        "after change, pgrp: {}",
        current_session_group_leader
            .task_pgrp()
            .unwrap()
            .pid_vnr()
            .data()
    );
}
