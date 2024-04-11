use crate::arch::ipc::signal::{SigCode, Signal};
use crate::ipc::signal_types::SigType;
use crate::libs::mutex::Mutex;
use crate::process::Pid;
use crate::process::SigInfo;
use crate::time::timer::{
    clock, n_ms_jiffies, next_n_jiffies_tiemr_jiffies, next_n_us_timer_jiffies, timer_jiffies_n_ms,
    Timer, TimerFunction,
};
use alloc::{boxed::Box, sync::Arc};
use core::sync::atomic::compiler_fence;
use core::time::Duration;
use system_error::SystemError;

use super::ProcessManager;

//Jiffies结构体表示一段时间的jiffies
pub struct Jiffies {
    jiffies: u64,
}

#[derive(Debug)]
pub struct AlarmTimer {
    timer: Mutex<Arc<Timer>>,
}

impl AlarmTimer {
    pub fn new(timer_func: Box<dyn TimerFunction>) -> Self {
        let expire_jiffies = next_n_us_timer_jiffies(0);
        let result = AlarmTimer {
            timer: Mutex::new(Timer::new(timer_func, expire_jiffies)),
        };
        result
    }

    pub fn activate(&self) {
        self.timer.lock().activate();
    }

    pub fn timeout(&self) -> bool {
        return self.timer.lock().timeout();
    }

    //返回闹钟定时器剩余时间
    pub fn remain(&self) -> Duration {
        if self.timeout() {
            Duration::ZERO
        } else {
            let now_time = clock();
            let end_time = self.timer.lock().inner().expire_jiffies;
            let remain_jiffies = Jiffies::new_from_jiffies(end_time - now_time);
            let second = remain_jiffies.jiffies_duration();
            second
        }
    }

    pub fn cancel(&self) {
        self.timer.lock().cancel();
    }

    pub fn restart(&self, jiffies: Jiffies) {
        let new_expired_jiffies = jiffies.expire_jiffies();
        let pid = ProcessManager::current_pid();
        let timerfunc = AlarmTimerFunc::new(pid);
        let new_timer = Timer::new(timerfunc, new_expired_jiffies);
        new_timer.activate();
        let mut timer = self.timer.lock();
        *timer = new_timer;
        drop(timer);
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
        let sig = Signal::SIGALRM;
        // 初始化signal info
        let mut info = SigInfo::new(sig, 0, SigCode::Timer, SigType::Alarm(self.pid));

        compiler_fence(core::sync::atomic::Ordering::SeqCst);

        let _retval = sig
            .send_signal_info(Some(&mut info), self.pid)
            .map(|x| x as usize)?;

        compiler_fence(core::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
}

//初始化目标进程的alarm定时器
pub fn alarm_timer_init(pid: Pid) -> AlarmTimer {
    //初始化Timerfunc
    let timerfunc = AlarmTimerFunc::new(pid);
    let alarmtimer = AlarmTimer::new(timerfunc);
    alarmtimer.activate();
    alarmtimer
}

impl Jiffies {
    //使用一段jiffies初始化
    pub fn new_from_jiffies(jiffies: u64) -> Self {
        let result = Jiffies { jiffies };
        result
    }
    //使用ms初始化
    pub fn new_from_duration(ms: Duration) -> Self {
        let jiffies = n_ms_jiffies(ms.as_micros() as u64);
        let result = Jiffies { jiffies };
        result
    }
    //返回jiffies
    pub fn inner_jiffies(&self) -> u64 {
        self.jiffies
    }
    //jiffies转一段时间duration
    pub fn jiffies_duration(&self) -> Duration {
        let ms = timer_jiffies_n_ms(self.jiffies);
        let result = Duration::from_micros(ms);
        result
    }
    //返回一段jiffies对应的定时器时间片
    pub fn expire_jiffies(&self) -> u64 {
        let result = next_n_jiffies_tiemr_jiffies(self.inner_jiffies());
        result
    }
}
