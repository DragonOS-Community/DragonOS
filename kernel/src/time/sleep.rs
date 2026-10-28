use core::hint::spin_loop;

use alloc::{boxed::Box, sync::Arc};
use system_error::SystemError;

use crate::{
    arch::{CurrentIrqArch, CurrentTimeArch},
    exception::InterruptArch,
    libs::wait_queue::{Waiter, Waker},
    process::preempt::PreemptGuard,
    smp::core::smp_get_processor_id,
};

use super::{
    deadline::{self, DeadlineCallback, DeadlineEvent},
    timekeeping::monotonic_now,
    timer::{next_n_us_timer_jiffies, Timer, TimerFunction},
    PosixTimeSpec, TimeArch,
};

#[derive(Debug)]
struct SleepWake(Arc<Waker>);

impl DeadlineCallback for SleepWake {
    fn expire(&self, _now_ns: u64) -> Option<u64> {
        self.0.wake();
        None
    }
}

impl TimerFunction for SleepWake {
    fn run(&mut self) -> Result<(), SystemError> {
        self.0.wake();
        Ok(())
    }
}

/// Own cancellation on every return path, including signal and early wake.
enum SleepTimer {
    Deadline(Arc<DeadlineEvent>),
    Coarse(Arc<Timer>),
}

impl Drop for SleepTimer {
    fn drop(&mut self) {
        match self {
            Self::Deadline(event) => event.cancel(),
            Self::Coarse(timer) => {
                timer.cancel();
            }
        }
    }
}

fn arm_sleep(deadline: PosixTimeSpec, waker: &Arc<Waker>) -> Result<SleepTimer, SystemError> {
    let (owner, capable) = {
        let _irq_guard = unsafe { CurrentIrqArch::save_and_disable_irq() };
        (smp_get_processor_id(), deadline::supported())
    };
    // All allocation/reclaim happens after restoring IRQs. x86 can arm the
    // fixed owner remotely; RISC-V admits deadline users only on a single CPU.
    if capable {
        let callback = Arc::try_new(SleepWake(waker.clone())).map_err(|_| SystemError::ENOMEM)?;
        let event = DeadlineEvent::try_new(owner, callback).map_err(|_| SystemError::ENOMEM)?;
        match event.arm(deadline.to_ktime_ns()) {
            Ok(()) => return Ok(SleepTimer::Deadline(event)),
            Err(SystemError::EOPNOTSUPP_OR_ENOTSUP) => event.cancel(),
            Err(err) => return Err(err),
        }
    }

    let remaining = deadline.saturating_sub_timespec(&monotonic_now());
    let expiry = next_n_us_timer_jiffies(remaining.to_ktime_ns().div_ceil(1000));
    let callback = Box::try_new(SleepWake(waker.clone())).map_err(|_| SystemError::ENOMEM)?;
    let prepared = Timer::try_prepare(callback, expiry)?;
    let timer = prepared.timer();
    prepared.activate();
    Ok(SleepTimer::Coarse(timer))
}

/// Sleep in process context until the monotonic deadline or a pending signal.
/// Hardware stalls in atomic/shutdown contexts must use `busy_wait_us` instead.
pub fn nanosleep(sleep_time: PosixTimeSpec) -> Result<(), SystemError> {
    if !sleep_time.is_valid_timeout() {
        return Err(SystemError::EINVAL);
    }
    if sleep_time.is_empty() {
        return Ok(());
    }

    let deadline = monotonic_now().saturating_add_ktime(&sleep_time);
    let (waiter, waker) = Waiter::try_new_pair()?;
    loop {
        if deadline
            .saturating_sub_timespec(&monotonic_now())
            .is_empty()
        {
            return Ok(());
        }
        let timer = arm_sleep(deadline, &waker)?;
        // Preparation may itself consume the remaining interval.
        if deadline
            .saturating_sub_timespec(&monotonic_now())
            .is_empty()
        {
            return Ok(());
        }
        let result = waiter.wait(true);
        drop(timer);
        let interrupted = match result {
            Ok(()) => false,
            Err(SystemError::ERESTARTSYS) => true,
            Err(err) => return Err(err),
        };
        // Completion wins a race with signal delivery, as in Linux nanosleep.
        if deadline
            .saturating_sub_timespec(&monotonic_now())
            .is_empty()
        {
            return Ok(());
        }
        if interrupted {
            return Err(SystemError::ERESTARTSYS);
        }
        // Never complete on a non-expiry notification or restart a full
        // relative interval. Rearm only the original absolute deadline.
    }
}

/// Calibrated hardware stall, including when IRQs/clockevents are shut down.
/// This intentionally does not provide the process-sleep monotonic contract.
pub(crate) fn busy_wait_us(mut microseconds: u64) {
    // Bound multiplication and each modular deadline to a short interval.
    // A passed/tiny deadline (including an uncalibrated zero frequency) must
    // return rather than underflow a cycle budget into an enormous delay.
    while microseconds != 0 {
        let chunk = microseconds.min(1_000);
        // Keep each calibrated interval in one CPU's counter domain. Between
        // chunks, callers with IRQs enabled may migrate and re-anchor.
        let _preempt = PreemptGuard::new();
        let expires = CurrentTimeArch::cal_expire_cycles(chunk as usize * 1_000);
        while (CurrentTimeArch::get_cycles().wrapping_sub(expires) as isize) < 0 {
            spin_loop();
        }
        microseconds -= chunk;
    }
}
