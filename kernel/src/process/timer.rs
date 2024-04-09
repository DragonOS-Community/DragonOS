use crate::arch::ipc::signal::{SigCode, Signal};
use crate::ipc::signal_types::SigType;
use crate::libs::mutex::Mutex;
use crate::process::Pid;
use crate::process::SigInfo;
use crate::time::timer::{clock, timer_jiffies_n_s, Timer, TimerFunction};
use alloc::{boxed::Box, sync::Arc};
use core::result;
use core::sync::atomic::compiler_fence;
use system_error::SystemError;

use super::ProcessManager;
#[derive(Debug)]
pub struct AlarmTimer {
    timer: Mutex<Arc<Timer>>,
    expired_time: u64,
}

impl AlarmTimer {
    pub fn new(timer_func: Box<dyn TimerFunction>, expire_time: u64) -> Self {
        let result = AlarmTimer {
            timer: Mutex::new(Timer::new(timer_func, expire_time)),
            expired_time: expire_time,
        };
        result
    }

    pub fn timeout(&self) -> bool {
        return self.timer.lock().timeout();
    }

    //返回闹钟定时器剩余时间（单位是jiffies）
    pub fn remain(&self) -> u64 {
        if self.timeout() {
            0
        } else {
            let now_time = clock();
            let end_time = self.timer.lock().inner().expire_jiffies;
            let remain_jiffies = end_time - now_time;
            let second = timer_jiffies_n_s(remain_jiffies);
            second
        }
    }

    pub fn cancel(&self) {
        self.timer.lock().cancel();
    }

    pub fn restart(&self, new_expire_jiffies: u64) {
        let pid = ProcessManager::current_pid();
        let timerfunc = AlarmTimerFunc::new(pid);
        let new_timer = Timer::new(timerfunc, new_expire_jiffies);
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
pub fn alarm_timer_init(pid: Pid, expire_jiffies: u64) -> Arc<AlarmTimer> {
    //初始化Timerfunc
    let timerfunc = AlarmTimerFunc::new(pid);
    let alarmtimer = AlarmTimer::new(timerfunc, expire_jiffies);
    let result = Arc::new(alarmtimer);
    result
}