use alloc::sync::Arc;
use system_error::SystemError;

use crate::{
    arch::ipc::signal::{SigSet, Signal},
    mm::VirtAddr,
    process::{process_group::Pgid, Pid, ProcessFlags, ProcessManager},
    syscall::{
        user_access::{UserBufferReader, UserBufferWriter},
        Syscall,
    },
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

    /// ### 检查tty
    pub fn tty_check_change(tty: Arc<TtyCore>, sig: Signal) -> Result<(), SystemError> {
        let pcb = ProcessManager::current_pcb();

        if pcb.sig_info_irqsave().tty().is_none()
            || !Arc::ptr_eq(&pcb.sig_info_irqsave().tty().unwrap(), &tty)
        {
            return Ok(());
        }

        let pgid = pcb.pgid();

        let ctrl = tty.core().contorl_info_irqsave();
        let tty_pgid = ctrl.pgid;
        drop(ctrl);

        if tty_pgid.is_some() && tty_pgid.unwrap() != pgid {
            if pcb
                .sig_info_irqsave()
                .sig_blocked()
                .contains(SigSet::from(sig))
                || pcb.sig_struct_irqsave().handlers[sig as usize - 1].is_ignore()
            {
                // 忽略该信号
                if sig == Signal::SIGTTIN {
                    return Err(SystemError::EIO);
                }
            } else if ProcessManager::is_current_pgrp_orphaned() {
                return Err(SystemError::EIO);
            } else {
                Syscall::kill_process_group(pgid, sig)?;
                ProcessManager::current_pcb()
                    .flags()
                    .insert(ProcessFlags::HAS_PENDING_SIGNAL);
                log::debug!("job_ctrl_ioctl: kill. pgid: {pgid}, tty_pgid: {tty_pgid:?}");
                return Err(SystemError::ERESTARTSYS);
            }
        }

        Ok(())
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
            TtyIoctlCmd::TIOCSCTTY => Self::tiocsctty(real_tty),
            _ => {
                return Err(SystemError::ENOIOCTLCMD);
            }
        }
    }

    fn tiocsctty(real_tty: Arc<TtyCore>) -> Result<usize, SystemError> {
        let current = ProcessManager::current_pcb();
        // log::debug!("job_ctrl_ioctl: TIOCSCTTY,current: {:?}", current.pid());
        if current.is_session_leader()
            && real_tty.core().contorl_info_irqsave().session.unwrap() == current.sid()
        {
            return Ok(0);
        }

        if !current.is_session_leader() || current.sig_info_irqsave().tty().is_some() {
            return Err(SystemError::EPERM);
        }

        //todo 权限检查？
        // https://code.dragonos.org.cn/xref/linux-6.6.21/drivers/tty/tty_jobctrl.c#tiocsctty
        if let Some(sid) = real_tty.core().contorl_info_irqsave().session {
            //todo 目前只有一个tty设备，所以选择复用1号进程的tty，因此修改1号进程的tty暂时被允许
            if sid != Pid::new(1) {
                return Err(SystemError::EPERM);
            }
        }

        Self::proc_set_tty(real_tty);
        Ok(0)
    }

    fn tiocgpgrp(real_tty: Arc<TtyCore>, arg: usize) -> Result<usize, SystemError> {
        // log::debug!("job_ctrl_ioctl: TIOCGPGRP");
        let current = ProcessManager::current_pcb();
        if current.sig_info_irqsave().tty().is_some()
            && !Arc::ptr_eq(&current.sig_info_irqsave().tty().unwrap(), &real_tty)
        {
            return Err(SystemError::ENOTTY);
        }

        let mut user_writer = UserBufferWriter::new(
            VirtAddr::new(arg).as_ptr::<i32>(),
            core::mem::size_of::<i32>(),
            true,
        )?;

        user_writer.copy_one_to_user(
            &(real_tty
                .core()
                .contorl_info_irqsave()
                .pgid
                .unwrap_or(Pid::new(1))
                .data() as i32),
            0,
        )?;

        return Ok(0);
    }

    fn tiocgsid(real_tty: Arc<TtyCore>, arg: usize) -> Result<usize, SystemError> {
        // log::debug!("job_ctrl_ioctl: TIOCGSID");
        let current = ProcessManager::current_pcb();
        if current.sig_info_irqsave().tty().is_some()
            && !Arc::ptr_eq(&current.sig_info_irqsave().tty().unwrap(), &real_tty)
        {
            return Err(SystemError::ENOTTY);
        }

        let guard = real_tty.core().contorl_info_irqsave();
        if guard.session.is_none() {
            return Err(SystemError::ENOTTY);
        }
        let sid = guard.session.unwrap();
        drop(guard);

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

        let pgrp = user_reader.read_one_from_user::<i32>(0)?;

        let current = ProcessManager::current_pcb();

        let mut ctrl = real_tty.core().contorl_info_irqsave();

        if current.sig_info_irqsave().tty().is_none()
            || !Arc::ptr_eq(
                &current.sig_info_irqsave().tty().clone().unwrap(),
                &real_tty,
            )
            || ctrl.session.is_none()
            || ctrl.session.unwrap() != current.sid()
        {
            return Err(SystemError::ENOTTY);
        }

        let pg = ProcessManager::find_process_group(Pgid::from(*pgrp as usize));
        if pg.is_none() {
            return Err(SystemError::ESRCH);
        } else if !Arc::ptr_eq(&pg.unwrap().session().unwrap(), &current.session().unwrap()) {
            return Err(SystemError::EPERM);
        }

        ctrl.pgid = Some(Pid::from(*pgrp as usize));

        return Ok(0);
    }
}
