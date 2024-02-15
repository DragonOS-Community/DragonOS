use alloc::sync::Arc;
use system_error::SystemError;

use crate::{arch::ipc::signal::SigSet, process::ProcessManager, syscall::Syscall};

use super::tty_core::TtyCore;

pub struct TtyJobCtrlManager;

impl TtyJobCtrlManager {
    /// ### 设置当前进程的tty
    pub fn proc_set_tty(tty: Arc<TtyCore>) {
        let core = tty.core();
        let mut ctrl = core.contorl_info_irqsave();
        let pcb = ProcessManager::current_pcb();

        // todo 目前将pgid设置为pid
        ctrl.pgid = Some(pcb.pid());
        ctrl.session = Some(pcb.pid());

        assert!(pcb.sig_info_irqsave().tty().is_none());

        let mut singal = pcb.sig_info_mut();
        drop(ctrl);
        singal.set_tty(tty);
    }

    /// ### 检查tty
    pub fn tty_check_change(tty: Arc<TtyCore>, sig: SigSet) -> Result<(), SystemError> {
        let pcb = ProcessManager::current_pcb();

        if pcb.sig_info().tty().is_none() || !Arc::ptr_eq(&pcb.sig_info().tty().unwrap(), &tty) {
            return Ok(());
        }

        let core = tty.core();
        let ctrl = core.contorl_info_irqsave();

        // todo pgid
        let pgid = pcb.pid();
        let tty_pgid = ctrl.pgid;

        if tty_pgid.is_some() && tty_pgid.unwrap() != pgid {
            if pcb.sig_info_irqsave().sig_block().contains(sig)
                || pcb.sig_struct_irqsave().handlers[sig.bits() as usize].is_ignore()
            {
                // 忽略该信号
                if sig == SigSet::SIGTTIN {
                    return Err(SystemError::EIO);
                }
            } else {
                // 暂时使用kill而不是killpg
                Syscall::kill(pgid, sig.bits() as i32)?;
                return Err(SystemError::ERESTART);
            }
        }

        Ok(())
    }
}
