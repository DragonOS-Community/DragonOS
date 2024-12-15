use alloc::sync::Arc;
use system_error::SystemError;

use crate::{
    arch::ipc::signal::{SigSet, Signal},
    mm::VirtAddr,
    process::{Pid, ProcessFlags, ProcessManager},
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
        let mut ctrl = core.contorl_info_irqsave();
        let pcb = ProcessManager::current_pcb();

        ctrl.session = Some(pcb.basic().sid());

        assert!(pcb.sig_info_irqsave().tty().is_none());

        let mut singal = pcb.sig_info_mut();
        drop(ctrl);
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

        let core = tty.core();
        let ctrl = core.contorl_info_irqsave();

        // todo pgid
        let pgid = pcb.pid();
        let tty_pgid = ctrl.pgid;

        if tty_pgid.is_some() && tty_pgid.unwrap() != pgid {
            if pcb
                .sig_info_irqsave()
                .sig_blocked()
                .contains(SigSet::from_bits_truncate(1 << sig as u64))
                || pcb.sig_struct_irqsave().handlers[sig as usize - 1].is_ignore()
            {
                // 忽略该信号
                if sig == Signal::SIGTTIN {
                    return Err(SystemError::EIO);
                }
            } else {
                // 暂时使用kill而不是killpg
                Syscall::kill(pgid, sig as i32)?;
                ProcessManager::current_pcb()
                    .flags()
                    .insert(ProcessFlags::HAS_PENDING_SIGNAL);
                log::debug!("job_ctrl_ioctl: kill. pgid: {pgid}, tty_pgid: {tty_pgid:?}");
                return Err(SystemError::ERESTARTSYS);
            }
        }

        Ok(())
    }

    pub fn job_ctrl_ioctl(tty: Arc<TtyCore>, cmd: u32, arg: usize) -> Result<usize, SystemError> {
        match cmd {
            TtyIoctlCmd::TIOCSPGRP => {
                match Self::tty_check_change(tty.clone(), Signal::SIGTTOU) {
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

                let mut ctrl = tty.core().contorl_info_irqsave();

                if current.sig_info_irqsave().tty().is_none()
                    || !Arc::ptr_eq(&current.sig_info_irqsave().tty().clone().unwrap(), &tty)
                    || ctrl.session.is_none()
                    || ctrl.session.unwrap() != current.basic().sid()
                {
                    return Err(SystemError::ENOTTY);
                }

                ctrl.pgid = Some(Pid::new(*pgrp as usize));

                return Ok(0);
            }

            TtyIoctlCmd::TIOCGPGRP => {
                let current = ProcessManager::current_pcb();
                if current.sig_info_irqsave().tty().is_some()
                    && !Arc::ptr_eq(&current.sig_info_irqsave().tty().unwrap(), &tty)
                {
                    return Err(SystemError::ENOTTY);
                }

                let mut user_writer = UserBufferWriter::new(
                    VirtAddr::new(arg).as_ptr::<i32>(),
                    core::mem::size_of::<i32>(),
                    true,
                )?;

                user_writer.copy_one_to_user(
                    &(tty
                        .core()
                        .contorl_info_irqsave()
                        .pgid
                        .unwrap_or(Pid::new(0))
                        .data() as i32),
                    0,
                )?;

                return Ok(0);
            }

            _ => {
                return Err(SystemError::ENOIOCTLCMD);
            }
        }
    }
}
