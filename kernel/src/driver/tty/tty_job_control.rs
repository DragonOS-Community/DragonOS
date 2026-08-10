use alloc::sync::Arc;
use system_error::SystemError;

use crate::{
    arch::ipc::signal::{SigSet, Signal},
    mm::VirtAddr,
    process::{
        cred::CAPFlags,
        pid::{Pid, PidType},
        ProcessControlBlock, ProcessManager, ProcessSignalInfo, RawPid,
    },
    syscall::user_access::{UserBufferReader, UserBufferWriter},
};

use super::tty_core::{TtyCore, TtyIoctlCmd};

pub struct TtyJobCtrlManager;

impl TtyJobCtrlManager {
    /// ### 设置当前进程的tty
    pub fn proc_set_tty(tty: Arc<TtyCore>) {
        let pcb = ProcessManager::current_pcb();
        let _job_control_guard = tty.core().job_control_lock().lock();
        let _membership_guard = crate::process::pid::pid_membership_lock();
        if tty
            .core()
            .flags()
            .intersects(super::tty_core::TtyFlag::HUPPING | super::tty_core::TtyFlag::HUPPED)
        {
            return;
        }
        let mut ctrl = tty.core().contorl_info_irqsave();
        let mut signal = pcb.sig_info_mut();
        if !signal.is_session_leader || signal.tty().is_some() || ctrl.session.is_some() {
            return;
        }
        ctrl.set_info_by_pcb(pcb.clone());
        signal.set_tty(Some(tty.clone()));
    }

    /// ### 清除进程的tty
    ///
    /// https://code.dragonos.org.cn/xref/linux-6.6.21/drivers/tty/tty_jobctrl.c#75
    pub fn proc_clear_tty(pcb: &Arc<ProcessControlBlock>) {
        let mut singal = pcb.sig_info_mut();
        Self::__proc_clear_tty(&mut singal);
    }

    pub fn __proc_clear_tty(wguard: &mut ProcessSignalInfo) {
        // log::debug!("Clearing tty");
        wguard.set_tty(None);
    }

    pub fn remove_session_tty(tty: &Arc<TtyCore>) -> Option<Arc<Pid>> {
        let _job_control_guard = tty.core().job_control_lock().lock();
        Self::remove_session_tty_job_locked(tty, None)
    }

    pub(crate) fn remove_session_tty_job_locked(
        tty: &Arc<TtyCore>,
        expected_sid: Option<&Arc<Pid>>,
    ) -> Option<Arc<Pid>> {
        let _membership_guard = crate::process::pid::pid_membership_lock();
        Self::remove_session_tty_locked(tty, expected_sid)
    }

    pub fn remove_session_tty_if_owner(
        tty: &Arc<TtyCore>,
        expected_sid: &Arc<Pid>,
    ) -> Option<Arc<Pid>> {
        let _job_control_guard = tty.core().job_control_lock().lock();
        Self::remove_session_tty_job_locked(tty, Some(expected_sid))
    }

    /// The caller must hold PID_MEMBERSHIP_LOCK.
    fn remove_session_tty_locked(
        tty: &Arc<TtyCore>,
        expected_sid: Option<&Arc<Pid>>,
    ) -> Option<Arc<Pid>> {
        let mut ctrl = tty.core().contorl_info_irqsave();
        if expected_sid.is_some_and(|expected| {
            ctrl.session
                .as_ref()
                .is_none_or(|owner| !Arc::ptr_eq(owner, expected))
        }) {
            return None;
        }
        let sid = ctrl.session.take();
        let pgid = ctrl.pgid.take();
        drop(ctrl);
        if let Some(sid) = sid {
            Self::session_clear_tty_locked(sid);
        }
        pgid
    }

    /// ### 检查tty
    ///
    /// check for POSIX terminal changes
    ///
    /// Reference: https://code.dragonos.org.cn/xref/linux-6.6.21/drivers/tty/tty_jobctrl.c#33
    pub fn tty_check_change(tty: Arc<TtyCore>, sig: Signal) -> Result<(), SystemError> {
        let pcb = ProcessManager::current_pcb();
        let current_tty = pcb.sig_info_irqsave().tty();
        if current_tty.is_none() || !Arc::ptr_eq(&current_tty.unwrap(), &tty) {
            return Ok(());
        }

        let pgid = pcb.task_pgrp();

        let ctrl = tty.core().contorl_info_irqsave();
        let tty_pgid = ctrl.pgid.clone();
        drop(ctrl);

        // log::debug!(
        //     "tty_check_change: pid: {},pgid: {:?}, tty_pgid: {:?}",
        //     pcb.raw_pid().data(),
        //     pgid.as_ref().map(|p| p.pid_vnr().data()),
        //     tty_pgid.as_ref().map(|p| p.pid_vnr().data())
        // );
        if tty_pgid.is_some() && tty_pgid != pgid {
            let pgid = pgid.unwrap();
            if Self::sig_is_ignored(sig) {
                // 忽略该信号
                if sig == Signal::SIGTTIN {
                    return Err(SystemError::EIO);
                }
            } else if ProcessManager::is_current_pgrp_orphaned() {
                log::debug!("tty_check_change: orphaned pgrp");
                return Err(SystemError::EIO);
            } else {
                crate::ipc::kill::send_signal_to_pgid(&pgid, sig)?;
                log::debug!(
                    "job_ctrl_ioctl: kill. pgid: {}, tty_pgid: {:?}",
                    pgid.pid_vnr().data(),
                    tty_pgid.map(|p| p.pid_vnr().data())
                );
                return Err(SystemError::ERESTARTSYS);
            }
        }

        Ok(())
    }

    fn sig_is_ignored(sig: Signal) -> bool {
        let pcb = ProcessManager::current_pcb();
        let siginfo_guard = pcb.sig_info_irqsave();
        siginfo_guard.sig_blocked().contains(SigSet::from(sig))
            || pcb.sighand().handler(sig).unwrap().is_ignore()
    }

    pub fn job_ctrl_ioctl(
        real_tty: Arc<TtyCore>,
        cmd: u32,
        arg: usize,
    ) -> Result<usize, SystemError> {
        match cmd {
            TtyIoctlCmd::TIOCSPGRP => Self::tiocspgrp(real_tty, arg),
            TtyIoctlCmd::TIOCGPGRP => Self::tiocgpgrp(real_tty, arg),
            TtyIoctlCmd::TIOCGSID => Self::tiocgsid(real_tty, arg),
            TtyIoctlCmd::TIOCSCTTY => Self::tiocsctty(real_tty, arg),
            TtyIoctlCmd::TIOCNOTTY => Self::tiocnotty(real_tty),
            _ => {
                return Err(SystemError::ENOIOCTLCMD);
            }
        }
    }

    // https://code.dragonos.org.cn/xref/linux-6.6.21/drivers/tty/tty_jobctrl.c#tiocsctty
    fn tiocsctty(real_tty: Arc<TtyCore>, arg: usize) -> Result<usize, SystemError> {
        let current = ProcessManager::current_pcb();
        let _job_control_guard = real_tty.core().job_control_lock().lock();
        let _membership_guard = crate::process::pid::pid_membership_lock();
        if real_tty
            .core()
            .flags()
            .intersects(super::tty_core::TtyFlag::HUPPING | super::tty_core::TtyFlag::HUPPED)
        {
            return Err(SystemError::EIO);
        }
        let (is_session_leader, current_tty) = {
            let siginfo = current.sig_info_irqsave();
            (siginfo.is_session_leader, siginfo.tty())
        };

        // 只有会话首进程才能设置控制终端
        if !is_session_leader {
            return Err(SystemError::EPERM);
        }

        let current_session = current.task_session();

        // 如果当前进程已经有控制终端，则返回错误（除非是同一个tty且会话相同）
        if let Some(current_tty) = current_tty {
            let tty_session = real_tty.core().contorl_info_irqsave().session.clone();
            if Arc::ptr_eq(&current_tty, &real_tty) && current_session == tty_session {
                // 如果已经是当前tty且会话相同，直接返回成功
                return Ok(0);
            } else {
                // 已经有其他控制终端，返回错误
                return Err(SystemError::EPERM);
            }
        }

        let mut tty_ctrl_guard = real_tty.core().contorl_info_irqsave();

        if let Some(ref sid) = tty_ctrl_guard.session {
            // 如果当前进程是会话首进程，且tty的会话是当前进程的会话，则允许设置
            if current_session == Some(sid.clone()) {
                // 这是正常情况：会话首进程要设置自己会话的tty为控制终端
            } else {
                // tty被其他会话占用
                if arg == 1 {
                    // 强制窃取控制终端，需要CAP_SYS_ADMIN权限
                    let cred = current.cred();
                    if !cred.has_capability(CAPFlags::CAP_SYS_ADMIN) {
                        return Err(SystemError::EPERM);
                    }
                    Self::session_clear_tty_locked(sid.clone());
                } else {
                    return Err(SystemError::EPERM);
                }
            }
        }

        tty_ctrl_guard.set_info_by_pcb(current.clone());
        current.sig_info_mut().set_tty(Some(real_tty.clone()));
        drop(tty_ctrl_guard);
        Ok(0)
    }

    fn tiocgpgrp(real_tty: Arc<TtyCore>, arg: usize) -> Result<usize, SystemError> {
        // log::debug!("job_ctrl_ioctl: TIOCGPGRP");
        let current = ProcessManager::current_pcb();
        let current_tty = current.sig_info_irqsave().tty();
        if current_tty.is_none() || !Arc::ptr_eq(&current_tty.unwrap(), &real_tty) {
            return Err(SystemError::ENOTTY);
        }

        let pgid = real_tty.core().contorl_info_irqsave().pgid.clone();
        let pgrp = pgid.map(|p| p.pid_vnr()).unwrap_or(RawPid::new(0)).data();
        // log::debug!("pid: {},tiocgpgrp: {}", current.raw_pid().data(), pgrp);
        let mut user_writer = UserBufferWriter::new(
            VirtAddr::new(arg).as_ptr::<i32>(),
            core::mem::size_of::<i32>(),
            true,
        )?;

        user_writer.copy_one_to_user(&(pgrp as i32), 0)?;

        return Ok(0);
    }

    /// Get session ID
    fn tiocgsid(real_tty: Arc<TtyCore>, arg: usize) -> Result<usize, SystemError> {
        // log::debug!("job_ctrl_ioctl: TIOCGSID");
        let current = ProcessManager::current_pcb();
        let current_tty = current.sig_info_irqsave().tty();
        if current_tty.is_none() || !Arc::ptr_eq(&current_tty.unwrap(), &real_tty) {
            return Err(SystemError::ENOTTY);
        }

        let guard = real_tty.core().contorl_info_irqsave();
        if guard.session.is_none() {
            return Err(SystemError::ENOTTY);
        }
        let session = guard.session.clone();
        // 先释放guard，免得阻塞了
        drop(guard);
        let sid = session.unwrap().pid_vnr();

        let mut user_writer = UserBufferWriter::new(
            VirtAddr::new(arg).as_ptr::<i32>(),
            core::mem::size_of::<i32>(),
            true,
        )?;
        user_writer.copy_one_to_user(&(sid.data() as i32), 0)?;

        return Ok(0);
    }

    fn tiocspgrp(real_tty: Arc<TtyCore>, arg: usize) -> Result<usize, SystemError> {
        // log::debug!("job_ctrl_ioctl: TIOCSPGRP");
        match Self::tty_check_change(real_tty.clone(), Signal::SIGTTOU) {
            Ok(_) => {}
            Err(e) => {
                if e == SystemError::EIO {
                    log::debug!("sss-1");
                    return Err(SystemError::ENOTTY);
                }
                return Err(e);
            }
        };

        let user_reader = UserBufferReader::new(
            VirtAddr::new(arg).as_ptr::<i32>(),
            core::mem::size_of::<i32>(),
            true,
        )?;

        let pgrp_nr = user_reader.read_one_from_user::<i32>(0)?;
        if pgrp_nr < 0 {
            return Err(SystemError::EINVAL);
        }

        let current = ProcessManager::current_pcb();
        let _job_control_guard = real_tty.core().job_control_lock().lock();
        let _membership_guard = crate::process::pid::pid_membership_lock();
        let current_session = current.task_session();
        let current_tty = current.sig_info_irqsave().tty();
        let mut ctrl = real_tty.core().contorl_info_irqsave();
        if current_tty
            .as_ref()
            .is_none_or(|tty| !Arc::ptr_eq(tty, &real_tty))
            || ctrl.session != current_session
        {
            return Err(SystemError::ENOTTY);
        }

        let pgrp =
            ProcessManager::find_vpid(RawPid::new(pgrp_nr as usize)).ok_or(SystemError::ESRCH)?;
        if Self::session_of_pgrp_locked(&pgrp) != current_session {
            return Err(SystemError::EPERM);
        }
        ctrl.pgid = Some(pgrp);

        return Ok(0);
    }

    /// The caller must hold PID_MEMBERSHIP_LOCK.
    fn session_of_pgrp_locked(pgrp: &Arc<Pid>) -> Option<Arc<Pid>> {
        let mut p = pgrp.pid_task(PidType::PGID);
        if p.is_none() {
            // this should not be None
            p = pgrp.pid_task(PidType::PID);
        }
        let p = p.unwrap();

        let sid = p.task_session();

        return sid;
    }

    /// Detach controlling tty from current process if it matches `real_tty`.
    fn tiocnotty(real_tty: Arc<TtyCore>) -> Result<usize, SystemError> {
        let pcb = ProcessManager::current_pcb();
        let (current_tty, is_session_leader) = {
            let siginfo = pcb.sig_info_irqsave();
            (siginfo.tty(), siginfo.is_session_leader)
        };

        if current_tty.is_none() || !Arc::ptr_eq(&current_tty.unwrap(), &real_tty) {
            return Err(SystemError::ENOTTY);
        }

        if !is_session_leader {
            Self::proc_clear_tty(&pcb);
            return Ok(0);
        }

        let tty_pgrp = if let Some(sid) = pcb.task_session() {
            Self::remove_session_tty_if_owner(&real_tty, &sid)
        } else {
            Self::proc_clear_tty(&pcb);
            None
        };

        if let Some(pgrp) = tty_pgrp {
            let _ = crate::ipc::kill::send_signal_to_pgid(&pgrp, Signal::SIGHUP);
            let _ = crate::ipc::kill::send_signal_to_pgid(&pgrp, Signal::SIGCONT);
        }

        Ok(0)
    }

    /// The caller must hold PID_MEMBERSHIP_LOCK.
    fn session_clear_tty_locked(sid: Arc<Pid>) {
        let leaders: alloc::vec::Vec<_> = sid.tasks_iter(PidType::SID).collect();
        for leader in leaders {
            for task in ProcessManager::thread_group_tasks_snapshot(leader) {
                TtyJobCtrlManager::proc_clear_tty(&task);
            }
        }
    }

    pub fn get_current_tty() -> Option<Arc<TtyCore>> {
        ProcessManager::current_pcb().sig_info_irqsave().tty()
    }
}
