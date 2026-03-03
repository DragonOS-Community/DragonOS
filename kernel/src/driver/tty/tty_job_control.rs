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
        let core = tty.core();
        let pcb = ProcessManager::current_pcb();

        let mut ctrl = core.contorl_info_irqsave();
        ctrl.set_info_by_pcb(pcb.clone());

        drop(ctrl);

        let mut singal = pcb.sig_info_mut();
        singal.set_tty(Some(tty.clone()));
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

    pub fn remove_session_tty(tty: &Arc<TtyCore>) {
        let mut ctrl = tty.core().contorl_info_irqsave();
        ctrl.session = None;
        ctrl.pgid = None;
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
        let siginfo_guard = current.sig_info_irqsave();

        // 只有会话首进程才能设置控制终端
        if !siginfo_guard.is_session_leader {
            return Err(SystemError::EPERM);
        }

        // 如果当前进程已经有控制终端，则返回错误（除非是同一个tty且会话相同）
        if let Some(current_tty) = siginfo_guard.tty() {
            if Arc::ptr_eq(&current_tty, &real_tty)
                && current.task_session() == real_tty.core().contorl_info_irqsave().session
            {
                // 如果已经是当前tty且会话相同，直接返回成功
                return Ok(0);
            } else {
                // 已经有其他控制终端，返回错误
                return Err(SystemError::EPERM);
            }
        }

        drop(siginfo_guard);

        let tty_ctrl_guard = real_tty.core().contorl_info_irqsave();

        if let Some(ref sid) = tty_ctrl_guard.session {
            // 如果当前进程是会话首进程，且tty的会话是当前进程的会话，则允许设置
            if current.task_session() == Some(sid.clone()) {
                // 这是正常情况：会话首进程要设置自己会话的tty为控制终端
            } else {
                // tty被其他会话占用
                if arg == 1 {
                    // 强制窃取控制终端，需要CAP_SYS_ADMIN权限
                    let cred = current.cred();
                    if !cred.has_capability(CAPFlags::CAP_SYS_ADMIN) {
                        return Err(SystemError::EPERM);
                    }
                    Self::session_clear_tty(sid.clone());
                } else {
                    return Err(SystemError::EPERM);
                }
            }
        }

        drop(tty_ctrl_guard);

        Self::proc_set_tty(real_tty);
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

        let pgrp_nr = *user_reader.read_one_from_user::<i32>(0)?;

        let current = ProcessManager::current_pcb();

        let mut ctrl = real_tty.core().contorl_info_irqsave();

        {
            // if current.sig_info_irqsave().tty().is_none()
            //     || !Arc::ptr_eq(
            //         &current.sig_info_irqsave().tty().clone().unwrap(),
            //         &real_tty,
            //     )
            //     || ctrl.session != current.task_session()
            // {
            //     log::debug!("sss-2");
            //     return Err(SystemError::ENOTTY);
            // }

            // 拆分判断条件以便调试
            let current_tty = current.sig_info_irqsave().tty();
            let condition1 = current_tty.is_none();

            let condition2 = if let Some(ref tty) = current_tty {
                !Arc::ptr_eq(tty, &real_tty)
            } else {
                false // 如果 tty 为 None，这个条件就不用检查了
            };

            let condition3 = ctrl.session != current.task_session();

            if condition1 || condition2 || condition3 {
                if condition1 {
                    log::debug!("sss-2: 失败原因 - 当前进程没有关联的 tty");
                } else if condition2 {
                    log::debug!("sss-2: 失败原因 - 当前进程的 tty 与目标 tty 不匹配");
                } else if condition3 {
                    log::debug!("sss-2: 失败原因 - 会话不匹配");
                }
                return Err(SystemError::ENOTTY);
            }
        }
        let pgrp =
            ProcessManager::find_vpid(RawPid::new(pgrp_nr as usize)).ok_or(SystemError::ESRCH)?;

        if Self::session_of_pgrp(&pgrp) != current.task_session() {
            return Err(SystemError::EPERM);
        }

        ctrl.pgid = Some(pgrp);

        return Ok(0);
    }

    fn session_of_pgrp(pgrp: &Arc<Pid>) -> Option<Arc<Pid>> {
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
        let mut siginfo = pcb.sig_info_mut();
        if let Some(cur) = siginfo.tty() {
            if Arc::ptr_eq(&cur, &real_tty) {
                Self::__proc_clear_tty(&mut siginfo);
                drop(siginfo);
                let mut ctrl = real_tty.core().contorl_info_irqsave();
                ctrl.session = None;
                ctrl.pgid = None;
                return Ok(0);
            }
        }
        Err(SystemError::ENOTTY)
    }

    pub(super) fn session_clear_tty(sid: Arc<Pid>) {
        // 清除会话的tty
        for task in sid.tasks_iter(PidType::SID) {
            TtyJobCtrlManager::proc_clear_tty(&task);
        }
    }

    pub fn get_current_tty() -> Option<Arc<TtyCore>> {
        ProcessManager::current_pcb().sig_info_irqsave().tty()
    }
}
