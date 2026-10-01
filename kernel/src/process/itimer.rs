//! Traditional interval timers belong to a thread group, not to the thread
//! which called setitimer. Lock order: itimers -> group pending/signal locks.
use alloc::{
    boxed::Box,
    sync::{Arc, Weak},
};
use system_error::SystemError;

use crate::{
    arch::ipc::signal::Signal,
    ipc::signal_types::{SigCode, SigInfo, SigType},
    libs::spinlock::SpinLock,
    process::{
        pid::{Pid, PidType},
        ProcessControlBlock, RawPid,
    },
    time::{
        jiffies::{NSEC_PER_JIFFY, TICK_NESC},
        syscall::{ItimerType, Itimerval, PosixTimeval},
        timer::{clock, Timer, TimerFunction},
    },
};

#[derive(Debug, Default)]
struct CpuItimer {
    remaining: u64,
    interval: u64,
}

impl CpuItimer {
    fn account(&mut self, delta: u64) -> bool {
        if self.remaining == 0 {
            return false;
        }
        if delta < self.remaining {
            self.remaining -= delta;
            return false;
        }
        self.remaining = if self.interval == 0 {
            0
        } else {
            self.interval - (delta - self.remaining) % self.interval
        };
        true
    }
}

#[derive(Debug, Default)]
struct RealItimer {
    timer: Option<Arc<Timer>>,
    /// Absolute expiry and interval in nanoseconds; queue rounding must not
    /// alter the configured interval or accumulate periodic phase drift.
    deadline: u64,
    interval: u64,
    generation: u64,
}

#[derive(Debug, Default)]
pub struct ProcessItimers {
    real: RealItimer,
    virt: CpuItimer,
    prof: CpuItimer,
    exited: bool,
}

// Match Linux timespec64_to_ktime/timeval conversion, including the seconds
// saturation threshold. A legal large timeval must never wrap or panic.
fn timeval_ns(value: PosixTimeval) -> u64 {
    if value.tv_sec >= i64::MAX / 1_000_000_000 {
        i64::MAX as u64
    } else {
        value.tv_sec as u64 * 1_000_000_000 + value.tv_usec as u64 * 1000
    }
}

fn ns_ticks(ns: u64) -> u64 {
    ns.div_ceil(NSEC_PER_JIFFY as u64)
}

fn clock_ns() -> u64 {
    (clock() as u128 * NSEC_PER_JIFFY as u128).min(i64::MAX as u128) as u64
}

/// No numeric PID lookup, caller identity or user kill permission check.
fn signal_group(pcb: Arc<ProcessControlBlock>, sig: Signal) -> Result<(), SystemError> {
    let mut info = SigInfo::new(
        sig,
        0,
        SigCode::Kernel,
        SigType::Kill {
            pid: RawPid::new(0),
            uid: 0,
        },
    );
    sig.send_signal_info_to_pcb(Some(&mut info), pcb, PidType::TGID)
        .map(|_| ())
}

impl ProcessItimers {
    pub fn get(&self, which: ItimerType) -> Itimerval {
        match which {
            ItimerType::Real => Itimerval {
                it_interval: PosixTimeval::from_ns(self.real.interval),
                it_value: if self.real.timer.is_some() {
                    let remaining = self.real.deadline.saturating_sub(clock_ns());
                    if remaining == 0 {
                        PosixTimeval {
                            tv_sec: 0,
                            tv_usec: 1,
                        }
                    } else {
                        PosixTimeval::from_ns(remaining.max(1000))
                    }
                } else {
                    PosixTimeval::default()
                },
            },
            ItimerType::Virtual | ItimerType::Prof => {
                let slot = if which == ItimerType::Virtual {
                    &self.virt
                } else {
                    &self.prof
                };
                Itimerval {
                    it_value: PosixTimeval::from_ns(slot.remaining),
                    it_interval: PosixTimeval::from_ns(slot.interval),
                }
            }
        }
    }

    fn queue_real(&mut self, state: &Arc<SpinLock<Self>>, target: Weak<Pid>) {
        let callback = Box::new(RealCallback {
            state: Arc::downgrade(state),
            target,
            generation: self.real.generation,
        });
        let timer = Timer::new(callback, ns_ticks(self.real.deadline));
        self.real.timer = Some(timer.clone());
        // Publish under the same state lock that the callback acquires.
        timer.activate();
    }

    fn set(
        &mut self,
        pcb: &ProcessControlBlock,
        which: ItimerType,
        config: Itimerval,
        target: Weak<Pid>,
    ) -> Itimerval {
        let old = self.get(which);
        let value = timeval_ns(config.it_value);
        let interval = timeval_ns(config.it_interval);
        match which {
            ItimerType::Real => {
                self.real.generation = self.real.generation.wrapping_add(1);
                if let Some(timer) = self.real.timer.take() {
                    timer.cancel();
                }
                self.real.interval = if value == 0 { 0 } else { interval };
                self.real.deadline = clock_ns().saturating_add(value).min(i64::MAX as u64);
                if value != 0 && !self.exited {
                    self.queue_real(&pcb.itimers, target);
                }
            }
            ItimerType::Virtual | ItimerType::Prof => {
                let slot = if which == ItimerType::Virtual {
                    &mut self.virt
                } else {
                    &mut self.prof
                };
                slot.interval = interval;
                slot.remaining = if value == 0 {
                    0
                } else {
                    value + TICK_NESC as u64
                };
            }
        }
        old
    }

    /// Caller holds the timer lock across shared SIGALRM dequeue and this
    /// restart; otherwise a newer expiry could be mistaken for the consumed one.
    pub(crate) fn restart_real(&mut self, pcb: &ProcessControlBlock) {
        if self.exited || self.real.timer.is_some() || self.real.interval == 0 {
            return;
        }
        let now = clock_ns();
        let elapsed = now.saturating_sub(self.real.deadline);
        let advance = self.real.interval - elapsed % self.real.interval;
        self.real.deadline = now.saturating_add(advance).min(i64::MAX as u64);
        let Some(target) = pcb.task_pid_ptr(PidType::TGID) else {
            return;
        };
        self.queue_real(&pcb.itimers, Arc::downgrade(&target));
    }

    pub(crate) fn exit(&mut self) {
        self.exited = true;
        self.real.generation = self.real.generation.wrapping_add(1);
        if let Some(timer) = self.real.timer.take() {
            timer.cancel();
        }
        self.real.interval = 0;
        self.virt = CpuItimer::default();
        self.prof = CpuItimer::default();
    }
}

impl ProcessControlBlock {
    pub fn get_itimer(&self, which: ItimerType) -> Itimerval {
        self.itimers_irqsave().get(which)
    }

    pub fn set_itimer(&self, which: ItimerType, config: Itimerval) -> Itimerval {
        let target = self
            .task_pid_ptr(PidType::TGID)
            .map(|pid| Arc::downgrade(&pid))
            .unwrap_or_default();
        self.itimers_irqsave().set(self, which, config, target)
    }

    pub(crate) fn account_itimers(self: &Arc<Self>, user: bool, delta: u64) {
        let mut state = self.itimers_irqsave();
        if state.exited {
            return;
        }
        if user && state.virt.account(delta) {
            let _ = signal_group(self.clone(), Signal::SIGVTALRM);
        }
        if state.prof.account(delta) {
            let _ = signal_group(self.clone(), Signal::SIGPROF);
        }
    }
}

#[derive(Debug)]
struct RealCallback {
    state: Weak<SpinLock<ProcessItimers>>,
    target: Weak<Pid>,
    generation: u64,
}

impl TimerFunction for RealCallback {
    fn run(&mut self) -> Result<(), SystemError> {
        let Some(state) = self.state.upgrade() else {
            return Ok(());
        };
        let mut state = state.lock_irqsave();
        if state.exited || state.real.generation != self.generation || state.real.timer.is_none() {
            return Ok(());
        }
        state.real.timer = None;
        // Hold the state lock until enqueue commits. Cancellation may leave an
        // already pending signal (Linux), but cannot race a later stale enqueue.
        if let Some(pid) = self.target.upgrade() {
            if let Some(pcb) = pid.pid_task(PidType::TGID) {
                signal_group(pcb, Signal::SIGALRM)?;
            }
        }
        Ok(())
    }
}
