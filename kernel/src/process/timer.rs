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

    //重启定时器
    pub fn reset(&mut self, new_expired_time: u64){
        println!("alarm ret!");
        let mut timer = self.inner();
        println!("old expired_jiffies: {}", timer.expire_jiffies);
        timer.expire_jiffies = new_expired_time;
        println!("new expired_jiffies: {}", timer.expire_jiffies);
        self.timer.restart();
        //重新插入到定时器列表
        self.timer.activate();
        drop(timer);
        println!("alarm reset success!");
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
    println!("begin init alarm!");
    let timerfunc = AlarmTimerFunc::new(pid);
    let result = AlarmTimer::new(timerfunc, time_out);
    let alarm = result.lock();
    let timer = alarm.as_ref();
    match timer {
        Some(timer) => {
            println!("alarm begin run!");
            timer.activate();
            println!("alarm run finish");
            //把alarm存放到pcb中
            let pcb_alarm = ProcessManager::ref_alarm_timer();
            let mut pcb_alarm_guard = pcb_alarm.lock();
            println!("clone begin");
            *pcb_alarm_guard = Some(timer.clone());
            println!("clone finish");
            //test
            match pcb_alarm_guard.as_ref() {
                Some(current_timer) => {
                    println!("current alarm's timeout: {}", current_timer.expired_time);
                }
                None => {
                    println!("alarm write in pcb wrong!");
                }
            }
        }
        None => {
            println!("alarm init wrong");
        }
    }
}