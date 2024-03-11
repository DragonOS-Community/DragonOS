use core::{
    fmt::Debug,
    intrinsics::unlikely,
    sync::atomic::{compiler_fence, AtomicBool, AtomicU64, Ordering},
};

use alloc::{
    boxed::Box,
    collections::LinkedList,
    sync::{Arc, Weak},
};
use system_error::SystemError;

use crate::{
    arch::{sched::sched, CurrentIrqArch},
    exception::{
        softirq::{softirq_vectors, SoftirqNumber, SoftirqVec},
        InterruptArch,
    },
    kerror, kinfo,
    libs::spinlock::{SpinLock, SpinLockGuard},
    process::{ProcessControlBlock, ProcessManager},
};

use super::timekeeping::update_wall_time;

const MAX_TIMEOUT: i64 = i64::MAX;
const TIMER_RUN_CYCLE_THRESHOLD: usize = 20;
static TIMER_JIFFIES: AtomicU64 = AtomicU64::new(0);

lazy_static! {
    pub static ref TIMER_LIST: SpinLock<LinkedList<Arc<Timer>>> = SpinLock::new(LinkedList::new());
}

/// 定时器要执行的函数的特征
pub trait TimerFunction: Send + Sync + Debug {
    fn run(&mut self) -> Result<(), SystemError>;
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

            timer_list.push_back(inner_guard.self_ref.upgrade().unwrap());

            drop(inner_guard);
            drop(timer_list);
            compiler_fence(Ordering::SeqCst);

            return;
        }
        let mut split_pos: usize = 0;
        for (pos, elt) in timer_list.iter().enumerate() {
            if elt.inner().expire_jiffies > inner_guard.expire_jiffies {
                split_pos = pos;
                break;
            }
        }
        let mut temp_list: LinkedList<Arc<Timer>> = timer_list.split_off(split_pos);
        timer_list.push_back(inner_guard.self_ref.upgrade().unwrap());
        timer_list.append(&mut temp_list);
        drop(inner_guard);
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
            kerror!(
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
            .extract_if(|x| Arc::ptr_eq(&this_arc, x))
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
            // kdebug!("DoTimerSoftirq run");
            let timer_list = TIMER_LIST.try_lock_irqsave();
            if timer_list.is_err() {
                continue;
            }
            let mut timer_list = timer_list.unwrap();

            if timer_list.is_empty() {
                break;
            }

            let timer_list_front = timer_list.pop_front().unwrap();
            // kdebug!("to lock timer_list_front");
            let mut timer_list_front_guard = None;
            for _ in 0..10 {
                let x = timer_list_front.inner.try_lock_irqsave();
                if x.is_err() {
                    continue;
                }
                timer_list_front_guard = Some(x.unwrap());
            }
            if timer_list_front_guard.is_none() {
                continue;
            }
            let timer_list_front_guard = timer_list_front_guard.unwrap();
            if timer_list_front_guard.expire_jiffies > TIMER_JIFFIES.load(Ordering::SeqCst) {
                drop(timer_list_front_guard);
                timer_list.push_front(timer_list_front);
                break;
            }
            drop(timer_list_front_guard);
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
    kinfo!("timer initialized successfully");
}

/// 计算接下来n毫秒对应的定时器时间片
pub fn next_n_ms_timer_jiffies(expire_ms: u64) -> u64 {
    return TIMER_JIFFIES.load(Ordering::SeqCst) + 1000 * (expire_ms);
}
/// 计算接下来n微秒对应的定时器时间片
pub fn next_n_us_timer_jiffies(expire_us: u64) -> u64 {
    return TIMER_JIFFIES.load(Ordering::SeqCst) + (expire_us);
}

/// @brief 让pcb休眠timeout个jiffies
///
/// @param timeout 需要休眠的时间(单位：jiffies)
///
/// @return Ok(i64) 剩余需要休眠的时间(单位：jiffies)
///
/// @return Err(SystemError) 错误码
pub fn schedule_timeout(mut timeout: i64) -> Result<i64, SystemError> {
    // kdebug!("schedule_timeout");
    if timeout == MAX_TIMEOUT {
        ProcessManager::mark_sleep(true).ok();
        sched();
        return Ok(MAX_TIMEOUT);
    } else if timeout < 0 {
        kerror!("timeout can't less than 0");
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

        sched();
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
    // kdebug!("rs_timer_get_first_expire,timer_jif = {:?}", TIMER_JIFFIES);
    for _ in 0..10 {
        match TIMER_LIST.try_lock_irqsave() {
            Ok(timer_list) => {
                // kdebug!("rs_timer_get_first_expire TIMER_LIST lock successfully");
                if timer_list.is_empty() {
                    // kdebug!("timer_list is empty");
                    return Ok(0);
                } else {
                    // kdebug!("timer_list not empty");
                    return Ok(timer_list.front().unwrap().inner().expire_jiffies);
                }
            }
            // 加锁失败返回啥？？
            Err(_) => continue,
        }
    }
    return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
}

/// 更新系统时间片
///
/// todo: 这里的实现有问题，貌似把HPET的500us当成了500个jiffies，然后update_wall_time()里面也硬编码了这个500us
pub fn update_timer_jiffies(add_jiffies: u64, time_us: i64) -> u64 {
    let prev = TIMER_JIFFIES.fetch_add(add_jiffies, Ordering::SeqCst);
    compiler_fence(Ordering::SeqCst);
    update_wall_time(time_us);

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
