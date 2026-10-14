//! Thread-group owned POSIX timers, with stable objects across async delivery.
mod cpu;
mod notification;
mod wall;

use crate::{
    ipc::signal_types::{SigInfo, SigType, TimerSignalToken},
    libs::spinlock::SpinLock,
    process::ProcessControlBlock,
    time::{syscall::PosixClockID, PosixTimeSpec},
};
use alloc::sync::{Arc, Weak};
use core::mem::size_of;
use hashbrown::HashMap;
use notification::Notification;
use system_error::SystemError;

#[repr(C)]
#[derive(Default, Debug, Copy, Clone)]
pub struct PosixItimerspec {
    pub it_interval: PosixTimeSpec,
    pub it_value: PosixTimeSpec,
}
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct PosixSigevent {
    pub sigev_value: u64,
    pub sigev_signo: i32,
    pub sigev_notify: i32,
    pub sigev_notify_thread_id: i32,
    pub _pad: [u8; 44],
}
const _: [(); 64] = [(); size_of::<PosixSigevent>()];
pub const SIGEV_SIGNAL: i32 = 0;
pub const SIGEV_NONE: i32 = 1;
pub const SIGEV_THREAD: i32 = 2;
pub const SIGEV_THREAD_ID: i32 = 4;

#[derive(Debug, Default)]
pub struct ProcessPosixTimers {
    next_id: i32,
    next_serial: u64,
    timers: HashMap<i32, Arc<PosixIntervalTimer>>,
}
impl ProcessPosixTimers {
    fn allocate(&mut self) -> Result<(i32, u64), SystemError> {
        let id = self.next_id.max(0);
        self.next_id = id
            .checked_add(1)
            .ok_or(SystemError::EAGAIN_OR_EWOULDBLOCK)?;
        self.next_serial = self
            .next_serial
            .checked_add(1)
            .ok_or(SystemError::EAGAIN_OR_EWOULDBLOCK)?;
        Ok((id, self.next_serial))
    }
    pub fn get_timer(&self, id: i32) -> Result<Arc<PosixIntervalTimer>, SystemError> {
        self.timers.get(&id).cloned().ok_or(SystemError::EINVAL)
    }
    pub fn remove(&mut self, id: i32) -> Result<Arc<PosixIntervalTimer>, SystemError> {
        self.timers.remove(&id).ok_or(SystemError::EINVAL)
    }
    pub fn take_all(&mut self) -> HashMap<i32, Arc<PosixIntervalTimer>> {
        core::mem::take(&mut self.timers)
    }
    /// Timer allocation and pending admission are outside the registry lock;
    /// the fallible map reservation and publication use one short critical section.
    pub fn create(
        pcb: &Arc<ProcessControlBlock>,
        clock: PosixClockID,
        event: Option<PosixSigevent>,
    ) -> Result<i32, SystemError> {
        let group = pcb.process_signal();
        let (id, serial) = group.posix_timers().lock_irqsave().allocate()?;
        let timer = PosixIntervalTimer::create(pcb, id, serial, clock, event)?;
        let mut registry = group.posix_timers().lock_irqsave();
        if registry.timers.try_reserve(1).is_err() {
            drop(registry);
            timer.delete();
            return Err(SystemError::ENOMEM);
        }
        registry.timers.insert(id, timer);
        Ok(id)
    }
}

enum Backend {
    Cpu(cpu::CpuBackend),
    Wall(wall::WallBackend),
}
impl core::fmt::Debug for Backend {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(if matches!(self, Self::Cpu(_)) {
            "Cpu"
        } else {
            "Wall"
        })
    }
}

#[derive(Debug, Default)]
struct State {
    deleted: bool,
    generation: u64,
    deadline: Option<u64>,
    interval: u64,
    pending: Option<u64>,
    next_delivery: u64,
    last_overrun: i32,
    overrun: u64,
    wall_event: Option<Arc<crate::time::timer::Timer>>,
}
#[derive(Debug)]
pub struct PosixIntervalTimer {
    pub id: i32,
    incarnation: u64,
    notify: Notification,
    backend: Backend,
    state: SpinLock<State>,
}
impl PosixIntervalTimer {
    fn create(
        pcb: &Arc<ProcessControlBlock>,
        id: i32,
        serial: u64,
        clock: PosixClockID,
        event: Option<PosixSigevent>,
    ) -> Result<Arc<Self>, SystemError> {
        let callback = Arc::try_new(Callback {
            timer: SpinLock::new(Weak::new()),
        })
        .map_err(|_| SystemError::ENOMEM)?;
        let backend = if matches!(clock, PosixClockID::Realtime | PosixClockID::Monotonic) {
            Backend::Wall(wall::WallBackend)
        } else {
            Backend::Cpu(cpu::CpuBackend::create(pcb, clock, callback.clone())?)
        };
        let notify = Notification::create(pcb, id, event)?;
        let timer = Arc::try_new(Self {
            id,
            incarnation: serial,
            notify,
            backend,
            state: SpinLock::new(State::default()),
        })
        .map_err(|_| SystemError::ENOMEM)?;
        *callback.timer.lock_irqsave() = Arc::downgrade(&timer);
        Ok(timer)
    }
    fn sample(&self) -> Option<u64> {
        match &self.backend {
            Backend::Cpu(cpu) => cpu.sample(),
            Backend::Wall(_) => Some(wall::now()),
        }
    }
    fn cancel(&self, state: &mut State) {
        match &self.backend {
            Backend::Cpu(cpu) => cpu.cancel(),
            Backend::Wall(_) => {
                if let Some(timer) = state.wall_event.take() {
                    timer.cancel();
                }
            }
        }
    }
    fn arm(self: &Arc<Self>, state: &mut State) -> Result<(), SystemError> {
        let Some(deadline) = state.deadline else {
            return Ok(());
        };
        match &self.backend {
            Backend::Cpu(cpu) => cpu.arm(deadline, state.generation),
            Backend::Wall(_) => {
                state.wall_event =
                    Some(wall::arm(Arc::downgrade(self), deadline, state.generation));
                Ok(())
            }
        }
    }
    fn current(&self, state: &State, now: Option<u64>) -> PosixItimerspec {
        let Some(now) = now else {
            return PosixItimerspec::default();
        };
        let mut value = state
            .deadline
            .map_or(0, |deadline| deadline.saturating_sub(now).max(1));
        if matches!(self.backend, Backend::Wall(_)) && state.interval > 0 && value == 1 {
            if let Some(deadline) = state.deadline {
                value = state.interval - now.saturating_sub(deadline) % state.interval;
            }
        }
        PosixItimerspec {
            it_value: PosixTimeSpec::from_ns(value),
            it_interval: PosixTimeSpec::from_ns(state.interval),
        }
    }
    pub fn gettime(&self) -> Result<PosixItimerspec, SystemError> {
        // SCHED sampling can acquire an rq lock. Never do it under timer state.
        let now = self.sample();
        let state = self.state.lock_irqsave();
        if state.deleted {
            return Err(SystemError::EINVAL);
        }
        Ok(self.current(&state, now))
    }
    pub fn getoverrun(&self) -> Result<i32, SystemError> {
        let state = self.state.lock_irqsave();
        if state.deleted {
            return Err(SystemError::EINVAL);
        }
        Ok(state.last_overrun)
    }
    pub fn settime(
        self: &Arc<Self>,
        flags: i32,
        new: PosixItimerspec,
    ) -> Result<PosixItimerspec, SystemError> {
        if matches!(self.backend, Backend::Wall(_)) && flags != 0 {
            return Err(SystemError::EINVAL);
        }
        let value = validate_ns(new.it_value)?;
        let interval = validate_ns(new.it_interval)?;
        let now = self.sample().ok_or(SystemError::ESRCH)?;
        if let Backend::Cpu(cpu) = &self.backend {
            cpu.validate_set()?;
        }
        let mut state = self.state.lock_irqsave();
        if state.deleted {
            return Err(SystemError::EINVAL);
        }
        let mut old = self.current(&state, Some(now));
        // Unlike gettime's due-but-not-yet-fired 1ns, settime reports the
        // old periodic setting with its next reload already projected.
        if matches!(self.backend, Backend::Cpu(_)) && state.interval != 0 {
            if let Some(deadline) = state.deadline.filter(|deadline| *deadline <= now) {
                let remaining = state.interval - (now - deadline) % state.interval;
                old.it_value = PosixTimeSpec::from_ns(remaining);
            }
        }
        self.cancel(&mut state);
        state.generation = state.generation.wrapping_add(1);
        state.interval = interval;
        state.deadline = if value == 0 {
            None
        } else {
            Some(if flags & 1 != 0 {
                value
            } else {
                now.saturating_add(value).min(i64::MAX as u64)
            })
        };
        state.overrun = 0;
        state.last_overrun = 0;
        // A previously queued signal remains, but cannot rearm a new setting.
        self.notify.reset_overrun(self.incarnation, self.id);
        let generation = state.generation;
        if state.deadline.is_some_and(|deadline| deadline <= now) {
            drop(state);
            self.expire(generation);
        } else {
            self.arm(&mut state)?;
        }
        Ok(old)
    }
    pub fn delete(&self) {
        let mut state = self.state.lock_irqsave();
        if state.deleted {
            return;
        }
        state.deleted = true;
        state.generation = state.generation.wrapping_add(1);
        self.cancel(&mut state);
        state.deadline = None;
        self.notify.reset_overrun(self.incarnation, self.id);
        self.notify.release();
    }
    fn advance(state: &mut State, now: u64) {
        let Some(deadline) = state.deadline else {
            return;
        };
        if state.interval == 0 {
            state.deadline = None;
            return;
        }
        let periods = now.saturating_sub(deadline) / state.interval + 1;
        state.overrun = state.overrun.saturating_add(periods.saturating_sub(1));
        state.deadline = Some(deadline.saturating_add(periods.saturating_mul(state.interval)));
    }
    fn expire(self: &Arc<Self>, generation: u64) {
        let now = match &self.backend {
            Backend::Cpu(cpu) => cpu.domain.sample(cpu.target.kind),
            Backend::Wall(_) => wall::now(),
        };
        let ignored = self.notify.ignored();
        let mut state = self.state.lock_irqsave();
        if state.deleted || state.generation != generation || state.deadline.is_none() {
            return;
        }
        if let Backend::Cpu(cpu) = &self.backend {
            if cpu.target.task().is_none() {
                state.deadline = None;
                return;
            }
        }
        if self.notify.is_none() {
            if matches!(self.backend, Backend::Cpu(_)) {
                state.deadline = None;
            } else {
                Self::advance(&mut state, now);
                let _ = self.arm(&mut state);
            }
            return;
        }
        if ignored {
            Self::advance(&mut state, now);
            state.overrun = state.overrun.saturating_add(1);
            let _ = self.arm(&mut state);
            return;
        }
        let delivery = state.next_delivery.wrapping_add(1);
        let token = TimerSignalToken {
            incarnation: self.incarnation,
            generation,
            delivery,
        };
        match self.notify.deliver(
            self.id,
            token,
            if matches!(self.backend, Backend::Cpu(_)) {
                0
            } else {
                state.overrun.min(i32::MAX as u64) as i32
            },
        ) {
            Some(delivery) => {
                state.pending = Some(delivery);
                state.next_delivery = delivery;
                if matches!(self.backend, Backend::Wall(_)) {
                    state.overrun = 0;
                }
            }
            None => state.deadline = None,
        }
        if state.interval == 0 {
            state.deadline = None;
        }
        // CPU periodic timers wait for signal dequeue; wall timers retain their
        // existing periodic service and coalescing behavior.
        if matches!(self.backend, Backend::Wall(_)) && state.interval > 0 {
            Self::advance(&mut state, now);
            let _ = self.arm(&mut state);
        }
    }
    pub(crate) fn signal_dequeued(self: &Arc<Self>, token: TimerSignalToken, info: &mut SigInfo) {
        let now = match &self.backend {
            Backend::Cpu(cpu) => cpu.domain.sample(cpu.target.kind),
            Backend::Wall(_) => wall::now(),
        };
        let mut state = self.state.lock_irqsave();
        if token.incarnation != self.incarnation || state.pending != Some(token.delivery) {
            return;
        }
        state.pending = None;
        if state.deleted || token.generation != state.generation {
            return;
        }
        if matches!(self.backend, Backend::Cpu(_)) && state.interval > 0 && state.deadline.is_some()
        {
            Self::advance(&mut state, now);
            state.last_overrun = state.overrun.min(i32::MAX as u64) as i32;
            info.bump_posix_timer_overrun(self.id, state.last_overrun);
            state.overrun = 0;
            let _ = self.arm(&mut state);
        }
        if matches!(self.backend, Backend::Wall(_)) {
            if let SigType::PosixTimer { overrun, .. } = info.sig_type() {
                state.last_overrun = *overrun;
            }
        }
    }
}
impl Drop for PosixIntervalTimer {
    fn drop(&mut self) {
        self.delete();
    }
}
fn validate_ns(value: PosixTimeSpec) -> Result<u64, SystemError> {
    if !value.is_valid_timeout() {
        return Err(SystemError::EINVAL);
    }
    Ok((value.tv_sec as u64)
        .saturating_mul(1_000_000_000)
        .saturating_add(value.tv_nsec as u64)
        .min(i64::MAX as u64))
}
struct Callback {
    timer: SpinLock<Weak<PosixIntervalTimer>>,
}
impl crate::time::cpu_events::CpuEventCallback for Callback {
    fn expired(&self, generation: u64) {
        let timer = self.timer.lock_irqsave().upgrade();
        if let Some(timer) = timer {
            timer.expire(generation);
        }
    }
}
