use crate::arch::CurrentIrqArch;
use crate::exception::InterruptArch;
use crate::ipc::signal_types::{SigInfo, SigType};
use crate::process::pid::PidType;
use crate::process::ProcessControlBlock;
use crate::time::timer::{clock, Jiffies, Timer, TimerFunction};
use crate::{arch::ipc::signal::Signal, ipc::signal_types::SigCode};
use alloc::{boxed::Box, sync::Arc, sync::Weak};
use core::sync::atomic::compiler_fence;
use core::time::Duration;
use system_error::SystemError;

/// Alarm timer structure.
#[derive(Debug)]
pub struct AlarmTimer {
    /// Built-in timer of the alarm.
    pub timer: Arc<Timer>,
    /// Alarm trigger time.
    expired_second: u64,
}

impl AlarmTimer {
    /// # Create an alarm timer structure.
    ///
    /// Create an alarm timer struct with a custom timer trigger function and
    /// deadline.
    ///
    /// ## Parameters
    ///
    /// - `timer_func`: The timer trigger function.
    /// - `second`: The number of seconds until the alarm fires.
    ///
    /// ### Returns
    ///
    /// `Self`
    pub fn new(timer_func: Box<dyn TimerFunction>, second: u64) -> Self {
        let expired_jiffies =
            <Jiffies as From<Duration>>::from(Duration::from_secs(second)).timer_jiffies();
        let result = AlarmTimer {
            timer: Timer::new(timer_func, expired_jiffies),
            expired_second: second,
        };
        result
    }
    /// # Activate the alarm.
    pub fn activate(&self) {
        let timer = self.timer.clone();
        timer.activate();
    }

    /// # Initialize an alarm timer for a target process.
    ///
    /// Create an alarm timer structure and activate it.
    ///
    /// ## Parameters
    ///
    /// - `pcb`: The target process to send the signal to.
    /// - `second`: The number of seconds until the alarm fires.
    ///
    /// ### Returns
    ///
    /// An `AlarmTimer` struct.
    pub fn alarm_timer_init(pcb: Arc<ProcessControlBlock>, second: u64) -> AlarmTimer {
        let timerfunc = AlarmTimerFunc::new(pcb);
        let alarmtimer = AlarmTimer::new(timerfunc, second);
        alarmtimer.activate();
        alarmtimer
    }

    /// # Check whether the alarm has fired.
    pub fn timeout(&self) -> bool {
        self.timer.timeout()
    }

    /// # Returns the remaining time on the alarm timer.
    pub fn remain(&self) -> Duration {
        if self.timer.timeout() {
            Duration::ZERO
        } else {
            let now_jiffies = clock();
            let end_jiffies =
                <Jiffies as From<Duration>>::from(Duration::from_secs(self.expired_second))
                    .timer_jiffies();
            let remain_second = Duration::from(Jiffies::new(end_jiffies - now_jiffies));
            // debug!(
            //     "end: {} - now: {} = remain: {}",
            //     end_jiffies,
            //     now_jiffies,
            //     end_jiffies - now_jiffies
            // );
            remain_second
        }
    }
    /// # Cancel the alarm.
    pub fn cancel(&self) {
        self.timer.cancel();
    }
}

/// # Alarm `TimerFunction` struct.
///
/// ## Struct Members
///
/// - `target_pcb`: The target process to send the signal to (weak reference;
///   automatically invalidated after the process exits).
#[derive(Debug)]
pub struct AlarmTimerFunc {
    target_pcb: Weak<ProcessControlBlock>,
}

impl AlarmTimerFunc {
    pub fn new(pcb: Arc<ProcessControlBlock>) -> Box<AlarmTimerFunc> {
        Box::new(AlarmTimerFunc {
            target_pcb: Arc::downgrade(&pcb),
        })
    }
}

impl TimerFunction for AlarmTimerFunc {
    /// # Alarm trigger function.
    ///
    /// When the alarm fires, sends a SIGALRM signal to the target process.
    /// If the target process has already exited (weak reference upgrade fails),
    /// silently returns.
    fn run(&mut self) -> Result<(), SystemError> {
        let pcb = match self.target_pcb.upgrade() {
            Some(pcb) => pcb,
            None => return Ok(()), // Process has already exited; no signal needed.
        };

        let pid = pcb.raw_pid();
        let sig = Signal::SIGALRM;
        let mut info = SigInfo::new(sig, 0, SigCode::Timer, SigType::Alarm(pid));

        compiler_fence(core::sync::atomic::Ordering::SeqCst);
        let irq_guard = unsafe { CurrentIrqArch::save_and_disable_irq() };
        let _retval = sig
            .send_signal_info_to_pcb(Some(&mut info), pcb, PidType::PID)
            .map(|x| x as usize)?;
        compiler_fence(core::sync::atomic::Ordering::SeqCst);
        drop(irq_guard);
        Ok(())
    }
}
