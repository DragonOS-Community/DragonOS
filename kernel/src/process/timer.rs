use crate::arch::ipc::signal::{SigCode, Signal};
use crate::exception::InterruptArch;
use crate::ipc::signal_types::SigType;
use crate::process::CurrentIrqArch;
use crate::process::Pid;
use crate::process::SigInfo;
use crate::sched::{schedule, SchedMode};
use crate::time::timer::{clock, Jiffies, Timer, TimerFunction};
use alloc::{boxed::Box, sync::Arc};
use core::sync::atomic::compiler_fence;
use core::time::Duration;
use system_error::SystemError;

//Jiffies结构体表示一段时间的jiffies

#[derive(Debug)]
pub struct AlarmTimer {
    pub timer: Arc<Timer>,
    expired_second: u64,
}

impl AlarmTimer {
    pub fn new(timer_func: Box<dyn TimerFunction>, second: u64) -> Self {
        let expired_jiffies = Jiffies::from(Duration::from_secs(second)).timer_jiffies();
        let result = AlarmTimer {
            timer: Timer::new(timer_func, expired_jiffies),
            expired_second: second,
        };
        result
    }

    pub fn activate(&self) {
        let timer = self.timer.clone();
        timer.activate();
    }

    pub fn timeout(&self) -> bool {
        self.timer.timeout()
    }

    //返回闹钟定时器剩余时间
    pub fn remain(&self) -> Duration {
        if self.timer.timeout() {
            Duration::ZERO
        } else {
            let now_jiffies = clock();
            let end_jiffies =
                Jiffies::from(Duration::from_secs(self.expired_second)).timer_jiffies();
            let remain_second = Duration::from(Jiffies::new(end_jiffies - now_jiffies));
            // kdebug!(
            //     "end: {} - now: {} = remain: {}",
            //     end_jiffies,
            //     now_jiffies,
            //     end_jiffies - now_jiffies
            // );
            remain_second
        }
    }

    pub fn cancel(&self) {
        self.timer.cancel();
    }
}

//闹钟定时器的TimerFuntion
#[derive(Debug)]
pub struct AlarmTimerFunc {
    pid: Pid,
}

impl AlarmTimerFunc {
    pub fn new(pid: Pid) -> Box<AlarmTimerFunc> {
        return Box::new(AlarmTimerFunc { pid });
    }
}

impl TimerFunction for AlarmTimerFunc {
    fn run(&mut self) -> Result<(), SystemError> {
        kdebug!("run!");
        let sig = Signal::SIGALRM;
        // 初始化signal info
        let mut info = SigInfo::new(sig, 0, SigCode::Timer, SigType::Alarm(self.pid));

        compiler_fence(core::sync::atomic::Ordering::SeqCst);
        let irq_guard = unsafe { CurrentIrqArch::save_and_disable_irq() };
        let _retval = sig
            .send_signal_info(Some(&mut info), self.pid)
            .map(|x| x as usize)?;
        compiler_fence(core::sync::atomic::Ordering::SeqCst);
        drop(irq_guard);
        Ok(())
    }
}

//初始化目标进程的alarm定时器
//second是alarm设置的秒数
pub fn alarm_timer_init(pid: Pid, second: u64) -> AlarmTimer {
    //初始化Timerfunc
    let timerfunc = AlarmTimerFunc::new(pid);
    let alarmtimer = AlarmTimer::new(timerfunc, second);
    alarmtimer.activate();
    alarmtimer
}
