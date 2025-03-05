use core::{
    fmt::Debug,
    intrinsics::unlikely,
    sync::atomic::{compiler_fence, AtomicBool, AtomicU64, Ordering},
    time::Duration,
};

use alloc::{
    boxed::Box,
    sync::{Arc, Weak},
    vec::Vec,
};
use log::{error, info, warn};
use system_error::SystemError;

use crate::{
    arch::CurrentIrqArch,
    exception::{
        softirq::{softirq_vectors, SoftirqNumber, SoftirqVec},
        InterruptArch,
    },
    libs::spinlock::{SpinLock, SpinLockGuard},
    process::{ProcessControlBlock, ProcessManager},
    sched::{schedule, SchedMode},
};

use super::{jiffies::NSEC_PER_JIFFY, timekeeping::update_wall_time};

const MAX_TIMEOUT: i64 = i64::MAX;
const TIMER_RUN_CYCLE_THRESHOLD: usize = 20;
static TIMER_JIFFIES: AtomicU64 = AtomicU64::new(0);

lazy_static! {
    pub static ref TIMER_LIST: SpinLock<Vec<(u64, Arc<Timer>)>> = SpinLock::new(Vec::new());
}

/// 定时器要执行的函数的特征
pub trait TimerFunction: Send + Sync + Debug {
    fn run(&mut self) -> Result<(), SystemError>;
}
// # Jiffies结构体（注意这是一段时间的jiffies数而不是某一时刻的定时器时间片）

int_like!(Jiffies, u64);

impl Jiffies {
    /// ## 返回接下来的n_jiffies对应的定时器时间片
    pub fn timer_jiffies(&self) -> u64 {
        let result = TIMER_JIFFIES.load(Ordering::SeqCst) + self.data();
        result
    }
}

impl From<Jiffies> for Duration {
    /// # Jiffies转Duration
    ///
    /// ## 参数
    ///
    /// jiffies： 一段时间的jiffies数
    ///
    /// ### 返回值
    ///
    /// Duration： 这段时间的Duration形式
    fn from(jiffies: Jiffies) -> Self {
        let ms = jiffies.data() / 1_000_000 * NSEC_PER_JIFFY as u64;
        let result = Duration::from_millis(ms);
        result
    }
}

impl From<Duration> for Jiffies {
    /// # Duration 转 Jiffies
    ///
    /// ## 参数
    ///
    /// ms： 表示一段时间的Duration类型
    ///
    /// ### 返回值
    ///
    /// Jiffies结构体： 这段时间的Jiffies数
    fn from(ms: Duration) -> Self {
        let jiffies = ms.as_millis() as u64 * 1_000_000 / NSEC_PER_JIFFY as u64;
        let result = Jiffies::new(jiffies);
        result
    }
}

#[derive(Debug)]
/// WakeUpHelper函数对应的结构体
pub struct WakeUpHelper {
    pcb: Arc<ProcessControlBlock>,
}

impl WakeUpHelper {
    pub fn new(pcb: Arc<ProcessControlBlock>) -> Box<WakeUpHelper> {
        return Box::new(WakeUpHelper { pcb });
    }
}

impl TimerFunction for WakeUpHelper {
    fn run(&mut self) -> Result<(), SystemError> {
        ProcessManager::wakeup(&self.pcb).ok();
        return Ok(());
    }
}

#[derive(Debug)]
pub struct Timer {
    inner: SpinLock<InnerTimer>,
}

impl Timer {
    /// @brief 创建一个定时器（单位：ms）
    ///
    /// @param timer_func 定时器需要执行的函数对应的结构体
    ///
    /// @param expire_jiffies 定时器结束时刻
    ///
    /// @return 定时器结构体
    pub fn new(timer_func: Box<dyn TimerFunction>, expire_jiffies: u64) -> Arc<Self> {
        let result: Arc<Timer> = Arc::new(Timer {
            inner: SpinLock::new(InnerTimer {
                expire_jiffies,
                timer_func: Some(timer_func),
                self_ref: Weak::default(),
                triggered: false,
            }),
        });

        result.inner.lock().self_ref = Arc::downgrade(&result);

        return result;
    }

    pub fn inner(&self) -> SpinLockGuard<InnerTimer> {
        return self.inner.lock_irqsave();
    }

    /// @brief 将定时器插入到定时器链表中
    pub fn activate(&self) {
        let mut timer_list = TIMER_LIST.lock_irqsave();
        let inner_guard = self.inner();

        // 链表为空，则直接插入
        if timer_list.is_empty() {
            // FIXME push_timer
            timer_list.push((
                inner_guard.expire_jiffies,
                inner_guard.self_ref.upgrade().unwrap(),
            ));

            drop(inner_guard);
            drop(timer_list);
            compiler_fence(Ordering::SeqCst);

            return;
        }
        let expire_jiffies = inner_guard.expire_jiffies;
        let self_arc = inner_guard.self_ref.upgrade().unwrap();
        drop(inner_guard);
        let mut split_pos: usize = timer_list.len();
        for (pos, elt) in timer_list.iter().enumerate() {
            if Arc::ptr_eq(&self_arc, &elt.1) {
                warn!("Timer already in list");
            }
            if elt.0 > expire_jiffies {
                split_pos = pos;
                break;
            }
        }
        timer_list.insert(split_pos, (expire_jiffies, self_arc));

        drop(timer_list);
    }

    #[inline]
    fn run(&self) {
        let mut timer = self.inner();
        timer.triggered = true;
        let func = timer.timer_func.take();
        drop(timer);
        let r = func.map(|mut f| f.run()).unwrap_or(Ok(()));
        if unlikely(r.is_err()) {
            error!(
                "Failed to run timer function: {self:?} {:?}",
                r.as_ref().err().unwrap()
            );
        }
    }

    /// ## 判断定时器是否已经触发
    pub fn timeout(&self) -> bool {
        self.inner().triggered
    }

    /// ## 取消定时器任务
    pub fn cancel(&self) -> bool {
        let this_arc = self.inner().self_ref.upgrade().unwrap();
        TIMER_LIST
            .lock_irqsave()
            .extract_if(|x| Arc::ptr_eq(&this_arc, &x.1))
            .for_each(drop);
        true
    }
}

/// 定时器类型
#[derive(Debug)]
pub struct InnerTimer {
    /// 定时器结束时刻
    pub expire_jiffies: u64,
    /// 定时器需要执行的函数结构体
    pub timer_func: Option<Box<dyn TimerFunction>>,
    /// self_ref
    self_ref: Weak<Timer>,
    /// 判断该计时器是否触发
    triggered: bool,
}

#[derive(Debug)]
pub struct DoTimerSoftirq {
    running: AtomicBool,
}

impl DoTimerSoftirq {
    pub fn new() -> Self {
        return DoTimerSoftirq {
            running: AtomicBool::new(false),
        };
    }

    fn set_run(&self) -> bool {
        let x = self
            .running
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed);
        return x.is_ok();
    }

    fn clear_run(&self) {
        self.running.store(false, Ordering::Release);
    }
}
impl SoftirqVec for DoTimerSoftirq {
    fn run(&self) {
        if !self.set_run() {
            return;
        }
        // 最多只处理TIMER_RUN_CYCLE_THRESHOLD个计时器
        for _ in 0..TIMER_RUN_CYCLE_THRESHOLD {
            // debug!("DoTimerSoftirq run");
            let timer_list = TIMER_LIST.try_lock_irqsave();
            if timer_list.is_err() {
                continue;
            }
            let mut timer_list = timer_list.unwrap();

            if timer_list.is_empty() {
                break;
            }

            let (front_jiffies, timer_list_front) = timer_list.first().unwrap().clone();
            // debug!("to lock timer_list_front");

            if front_jiffies >= TIMER_JIFFIES.load(Ordering::SeqCst) {
                break;
            }
            timer_list.remove(0);
            drop(timer_list);
            timer_list_front.run();
        }

        self.clear_run();
    }
}

/// 初始化系统定时器
#[inline(never)]
pub fn timer_init() {
    // FIXME 调用register_trap
    let do_timer_softirq = Arc::new(DoTimerSoftirq::new());
    softirq_vectors()
        .register_softirq(SoftirqNumber::TIMER, do_timer_softirq)
        .expect("Failed to register timer softirq");
    info!("timer initialized successfully");
}

/// 计算接下来n毫秒对应的定时器时间片
pub fn next_n_ms_timer_jiffies(expire_ms: u64) -> u64 {
    return TIMER_JIFFIES.load(Ordering::SeqCst) + expire_ms * 1000000 / NSEC_PER_JIFFY as u64;
}
/// 计算接下来n微秒对应的定时器时间片
pub fn next_n_us_timer_jiffies(expire_us: u64) -> u64 {
    return TIMER_JIFFIES.load(Ordering::SeqCst) + expire_us * 1000 / NSEC_PER_JIFFY as u64;
}

/// @brief 让pcb休眠timeout个jiffies
///
/// @param timeout 需要休眠的时间(单位：jiffies)
///
/// @return Ok(i64) 剩余需要休眠的时间(单位：jiffies)
///
/// @return Err(SystemError) 错误码
pub fn schedule_timeout(mut timeout: i64) -> Result<i64, SystemError> {
    // debug!("schedule_timeout");
    if timeout == MAX_TIMEOUT {
        let irq_guard = unsafe { CurrentIrqArch::save_and_disable_irq() };
        ProcessManager::mark_sleep(true).ok();
        drop(irq_guard);
        schedule(SchedMode::SM_NONE);
        return Ok(MAX_TIMEOUT);
    } else if timeout < 0 {
        error!("timeout can't less than 0");
        return Err(SystemError::EINVAL);
    } else {
        // 禁用中断，防止在这段期间发生调度，造成死锁
        let irq_guard = unsafe { CurrentIrqArch::save_and_disable_irq() };

        timeout += TIMER_JIFFIES.load(Ordering::SeqCst) as i64;
        let timer = Timer::new(
            WakeUpHelper::new(ProcessManager::current_pcb()),
            timeout as u64,
        );
        ProcessManager::mark_sleep(true).ok();
        timer.activate();

        drop(irq_guard);

        schedule(SchedMode::SM_NONE);
        let time_remaining: i64 = timeout - TIMER_JIFFIES.load(Ordering::SeqCst) as i64;
        if time_remaining >= 0 {
            // 被提前唤醒，返回剩余时间
            return Ok(time_remaining);
        } else {
            return Ok(0);
        }
    }
}

pub fn timer_get_first_expire() -> Result<u64, SystemError> {
    // FIXME
    // debug!("rs_timer_get_first_expire,timer_jif = {:?}", TIMER_JIFFIES);
    for _ in 0..10 {
        match TIMER_LIST.try_lock_irqsave() {
            Ok(timer_list) => {
                // debug!("rs_timer_get_first_expire TIMER_LIST lock successfully");
                if timer_list.is_empty() {
                    // debug!("timer_list is empty");
                    return Ok(0);
                } else {
                    // debug!("timer_list not empty");
                    return Ok(timer_list.first().unwrap().0);
                }
            }
            // 加锁失败返回啥？？
            Err(_) => continue,
        }
    }
    return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
}

/// 检查是否需要触发定时器软中断，如果需要则触发
pub fn try_raise_timer_softirq() {
    if let Ok(first_expire) = timer_get_first_expire() {
        if first_expire <= clock() {
            softirq_vectors().raise_softirq(SoftirqNumber::TIMER);
        }
    }
}

/// 处理本地定时器中断
pub fn run_local_timer() {
    assert!(!CurrentIrqArch::is_irq_enabled());
    try_raise_timer_softirq();
}

/// 更新系统时间片
pub fn update_timer_jiffies(add_jiffies: u64) -> u64 {
    let prev = TIMER_JIFFIES.fetch_add(add_jiffies, Ordering::SeqCst);
    compiler_fence(Ordering::SeqCst);
    update_wall_time();

    compiler_fence(Ordering::SeqCst);
    return prev + add_jiffies;
}

pub fn clock() -> u64 {
    return TIMER_JIFFIES.load(Ordering::SeqCst);
}

// ====== 以下为给C提供的接口 ======

#[no_mangle]
pub extern "C" fn rs_timer_init() {
    timer_init();
}
