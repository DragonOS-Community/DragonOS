use crate::arch::ipc::signal::{SigCode, Signal};
use crate::exception::InterruptArch;
use crate::ipc::signal_types::SigType;
use crate::process::CurrentIrqArch;
use crate::process::Pid;
use crate::process::SigInfo;
use crate::time::timer::{clock, Jiffies, Timer, TimerFunction};
use alloc::{boxed::Box, sync::Arc};
use core::sync::atomic::compiler_fence;
use core::time::Duration;
use system_error::SystemError;

/// 闹钟结构体
#[derive(Debug)]
pub struct AlarmTimer {
    /// 闹钟内置定时器
    pub timer: Arc<Timer>,
    /// 闹钟触发时间
    expired_second: u64,
}

impl AlarmTimer {
    /// # 创建闹钟结构体
    ///  
    /// 自定义定时器触发函数和截止时间来创建闹钟结构体
    ///
    /// ## 函数参数
    ///
    /// timer_func：定时器触发函数
    ///
    /// second：设置alarm触发的秒数
    ///
    /// ### 函数返回值
    ///
    /// Self
    pub fn new(timer_func: Box<dyn TimerFunction>, second: u64) -> Self {
        let expired_jiffies =
            <Jiffies as From<Duration>>::from(Duration::from_secs(second)).timer_jiffies();
        let result = AlarmTimer {
            timer: Timer::new(timer_func, expired_jiffies),
            expired_second: second,
        };
        result
    }
    /// # 启动闹钟
    pub fn activate(&self) {
        let timer = self.timer.clone();
        timer.activate();
    }

    /// # 初始化目标进程的alarm定时器
    ///  
    /// 创建一个闹钟结构体并启动闹钟
    ///
    /// ## 函数参数
    ///
    /// pid：发送消息的目标进程的pid
    ///
    /// second：设置alarm触发的秒数
    ///
    /// ### 函数返回值
    ///
    /// AlarmTimer结构体
    pub fn alarm_timer_init(pid: Pid, second: u64) -> AlarmTimer {
        //初始化Timerfunc
        let timerfunc = AlarmTimerFunc::new(pid);
        let alarmtimer = AlarmTimer::new(timerfunc, second);
        alarmtimer.activate();
        alarmtimer
    }

    /// # 查看闹钟是否触发
    pub fn timeout(&self) -> bool {
        self.timer.timeout()
    }

    /// # 返回闹钟定时器剩余时间
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
    /// # 取消闹钟
    pub fn cancel(&self) {
        self.timer.cancel();
    }
}

/// # 闹钟TimerFuntion结构体
///
/// ## 结构成员
///
/// pid：发送消息的目标进程的pid
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
    /// # 闹钟触发函数
    ///  
    /// 闹钟触发时，向目标进程发送一个SIGALRM信号
    ///
    /// ## 函数参数
    ///
    /// expired_second：设置alarm触发的秒数
    ///
    /// ### 函数返回值
    ///
    /// Ok(()): 发送成功
    fn run(&mut self) -> Result<(), SystemError> {
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
