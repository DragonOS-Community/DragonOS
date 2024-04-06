use alloc::{
    boxed::Box,
    sync::Arc,
};
use system_error::SystemError;
use crate::arch::ipc::signal::{Signal,SigCode};
use crate::ipc::signal_types::SigType;
use crate::time::timer::{clock, timer_jiffies_n_s, InnerTimer, Timer, TimerFunction};
use crate::libs::spinlock::SpinLockGuard;
use crate::process::Pid;
use core::sync::atomic::compiler_fence;
use crate::libs::mutex::Mutex;
use crate::process::SigInfo;

use super::ProcessManager;
#[derive(Debug,Clone)]
pub struct AlarmTimer{
    timer: Arc<Timer>,
    expired_time: u64,
}

impl AlarmTimer {
    pub fn new(timer_func: Box<dyn TimerFunction>, expire_time: u64) -> Mutex<Option<Self>>{
        let result: Mutex<Option<Self>> = Mutex::new(Some(AlarmTimer{
            timer: Timer::new(timer_func, expire_time),
            expired_time:  expire_time,
        }));
        result
    }

    pub fn inner(&self) -> SpinLockGuard<InnerTimer> {
        return self.timer.inner();
    }

    pub fn activate(&self) {
        self.timer.activate();
    }

    pub fn timeout(&self) -> bool {
        return self.timer.timeout();
    }

    //返回闹钟定时器剩余时间（单位是jiffies）
    pub fn remain(&self) -> u64{
        if self.timeout() || self.inner().expire_jiffies == 0{
            0
        }
        else {
            let now_time = clock();
            let end_time = self.expired_time;
            let remain_jiffies = end_time - now_time;
            let second = timer_jiffies_n_s(remain_jiffies);
            second
        }
    }

    pub fn cancel(&self) {
        self.timer.cancel();
    }

}

//闹钟定时器的TimerFuntion
#[derive(Debug)]
pub struct AlarmTimerFunc{
    pid: Pid,
}

impl AlarmTimerFunc{
    pub fn new(pid: Pid) -> Box<AlarmTimerFunc> {
        return Box::new(AlarmTimerFunc{ 
            pid });
    }
}

impl TimerFunction for AlarmTimerFunc {
    fn run(&mut self) -> Result<(), SystemError>{
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
pub fn alarm_timer_init(pid: Pid, time_out: u64) {
    //初始化Timerfunc
    let timerfunc = AlarmTimerFunc::new(pid);
    let result = AlarmTimer::new(timerfunc, time_out);
    let alarm = result.lock();
    let timer = alarm.as_ref();
    match timer {
        Some(timer) => {
            timer.activate();
            //把alarm存放到pcb中
            let pcb_alarm = ProcessManager::ref_alarm_timer();
            let mut pcb_alarm_guard = pcb_alarm.lock();
            *pcb_alarm_guard = Some(timer.clone());
            drop(pcb_alarm_guard);
        }
        None => {
            println!("alarm init wrong");
        }
    }
}