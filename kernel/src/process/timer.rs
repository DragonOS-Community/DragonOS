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
#[derive(Debug)]
pub struct AlarmTimer{
    timer: Arc<Timer>,
    expired_time: u64,
}

impl AlarmTimer {
    pub fn new(timer_func: Box<dyn TimerFunction>, expire_time: u64) -> Mutex<Self>{
        let result: Mutex<Self> = Mutex::new(AlarmTimer{
            timer: Timer::new(timer_func, expire_time),
            expired_time:  expire_time,
        });
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
    pub fn reset(&self, new_expired_time: u64){
        let mut timer = self.inner();
        timer.expire_jiffies = new_expired_time;
        self.timer.restart();
        //重新插入到定时器列表
        self.timer.activate();
        drop(timer);
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
        if sig == Signal::INVALID {
            // 传入的signal数值不合法
            kwarn!("Not a valid signal number");
            return Err(SystemError::EINVAL);
        }
        // 初始化signal info
        let mut info = SigInfo::new(sig, 0, SigCode::Timer, SigType::Alarm(self.pid));

        compiler_fence(core::sync::atomic::Ordering::SeqCst);

        let retval = sig
            .send_signal_info(Some(&mut info), self.pid)
            .map(|x| x as usize)?;

        compiler_fence(core::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
}