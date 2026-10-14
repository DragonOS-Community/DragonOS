//! Stable signal destination and one pre-reserved FIFO slot per timer.
use super::{PosixSigevent, SIGEV_NONE, SIGEV_SIGNAL, SIGEV_THREAD, SIGEV_THREAD_ID};
use crate::{
    arch::ipc::signal::Signal,
    ipc::signal_types::{PosixSigval, SigCode, SigInfo, SigPending, SigType, TimerSignalToken},
    process::{
        pid::{Pid, PidType},
        ProcessControlBlock, ProcessFlags, ProcessManager, RawPid,
    },
};
use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, Ordering};
use system_error::SystemError;
#[derive(Debug)]
pub(super) enum Notification {
    None,
    Signal {
        pid: Arc<Pid>,
        thread: bool,
        signo: Signal,
        sigval: PosixSigval,
        reserved: AtomicBool,
    },
}
impl Notification {
    pub(super) fn create(
        pcb: &Arc<ProcessControlBlock>,
        id: i32,
        event: Option<PosixSigevent>,
    ) -> Result<Self, SystemError> {
        let event = event.unwrap_or(PosixSigevent {
            sigev_value: id as u64,
            sigev_signo: Signal::SIGALRM as i32,
            sigev_notify: SIGEV_SIGNAL,
            sigev_notify_thread_id: 0,
            _pad: [0; 44],
        });
        if event.sigev_notify == SIGEV_NONE {
            return Ok(Self::None);
        }
        if !matches!(
            event.sigev_notify,
            SIGEV_SIGNAL | SIGEV_THREAD | SIGEV_THREAD_ID
        ) {
            return Err(SystemError::EINVAL);
        }
        let signo = Signal::from(event.sigev_signo as usize);
        if !signo.is_valid() {
            return Err(SystemError::EINVAL);
        }
        let thread = event.sigev_notify == SIGEV_THREAD_ID;
        let pid = if thread {
            let pid = ProcessManager::find_vpid(RawPid::new(event.sigev_notify_thread_id as usize))
                .ok_or(SystemError::EINVAL)?;
            let task = pid.pid_task(PidType::PID).ok_or(SystemError::EINVAL)?;
            if !Arc::ptr_eq(&task.process_signal(), &pcb.process_signal()) {
                return Err(SystemError::EINVAL);
            }
            pid
        } else {
            pcb.task_pid_ptr(PidType::TGID).ok_or(SystemError::EINVAL)?
        };
        let result = Self::Signal {
            pid,
            thread,
            signo,
            sigval: PosixSigval::from_ptr(event.sigev_value),
            reserved: AtomicBool::new(false),
        };
        result
            .with_pending(|pending| pending.queue_mut().reserve_timer())
            .ok_or(SystemError::EINVAL)??;
        if let Self::Signal { reserved, .. } = &result {
            reserved.store(true, Ordering::Release);
        }
        Ok(result)
    }
    pub(super) fn is_none(&self) -> bool {
        matches!(self, Self::None)
    }
    fn target(&self) -> Option<Arc<ProcessControlBlock>> {
        match self {
            Self::None => None,
            Self::Signal { pid, thread, .. } => {
                pid.pid_task(if *thread { PidType::PID } else { PidType::TGID })
            }
        }
    }
    fn with_pending<R>(&self, f: impl FnOnce(&mut SigPending) -> R) -> Option<R> {
        let target = self.target()?;
        match self {
            Self::Signal { thread: true, .. } => Some(f(target.sig_info_mut().sig_pending_mut())),
            Self::Signal { .. } => Some(f(&mut target.process_signal().lock_irqsave())),
            Self::None => None,
        }
    }
    pub(super) fn release(&self) {
        if let Self::Signal { reserved, .. } = self {
            if reserved.swap(false, Ordering::AcqRel) {
                self.with_pending(|pending| pending.queue_mut().release_timer());
            }
        }
    }
    pub(super) fn ignored(&self) -> bool {
        let Self::Signal { signo, .. } = self else {
            return false;
        };
        let Some(target) = self.target() else {
            return false;
        };
        let info = target.sig_info_irqsave();
        let mut blocked = *info.sig_blocked();
        if target.flags().contains(ProcessFlags::RESTORE_SIG_MASK) {
            blocked.insert(*info.saved_sigmask());
        }
        let blocked = blocked.contains(signo.into_sigset());
        drop(info);
        !blocked
            && target
                .sighand()
                .handler(*signo)
                .is_some_and(|handler| handler.is_ignore())
    }
    pub(super) fn reset_overrun(&self, serial: u64, id: i32) {
        self.with_pending(|pending| {
            for info in pending.queue_mut().q.iter_mut() {
                if info
                    .timer_token()
                    .is_some_and(|token| token.incarnation == serial)
                {
                    info.reset_posix_timer_overrun(id);
                }
            }
        });
    }
    pub(super) fn deliver(
        &self,
        id: i32,
        mut token: TimerSignalToken,
        overrun: i32,
    ) -> Option<u64> {
        let Self::Signal {
            signo,
            sigval,
            thread,
            ..
        } = self
        else {
            return None;
        };
        let target = self.target()?;
        let result = self.with_pending(|pending| {
            // Preallocated POSIX signals coalesce by timer, not by signo.
            for info in pending.queue_mut().q.iter_mut() {
                if let Some(old) = info
                    .timer_token()
                    .filter(|old| old.incarnation == token.incarnation)
                {
                    token.delivery = old.delivery;
                    info.set_timer_token(token);
                    info.bump_posix_timer_overrun(id, overrun.saturating_add(1));
                    return Some(token.delivery);
                }
            }
            let mut info = SigInfo::new(
                *signo,
                0,
                SigCode::Timer,
                SigType::PosixTimer {
                    timerid: id,
                    overrun,
                    sigval: *sigval,
                },
            );
            info.set_timer_token(token);
            if pending.queue_mut().push_reserved_timer(info).is_err() {
                return None;
            }
            pending.signal_mut().insert((*signo).into());
            Some(token.delivery)
        })?;
        if result.is_some() {
            signo.complete_signal(
                target,
                if *thread { PidType::PID } else { PidType::TGID },
                true,
            );
        }
        result
    }
}
impl Drop for Notification {
    fn drop(&mut self) {
        self.release();
    }
}
