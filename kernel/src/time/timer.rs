use core::sync::atomic::{compiler_fence, AtomicBool, Ordering};

use alloc::{
    boxed::Box,
    collections::LinkedList,
    sync::{Arc, Weak},
};

use crate::{
    arch::{
        asm::current::current_pcb,
        interrupt::{cli, sti},
        sched::sched,
    },
    exception::softirq::{softirq_vectors, SoftirqNumber, SoftirqVec},
    include::bindings::bindings::{process_control_block, process_wakeup, pt_regs, PROC_RUNNING},
    kdebug, kerror,
    libs::spinlock::SpinLock,
    syscall::SystemError,
};

const MAX_TIMEOUT: i64 = i64::MAX;
const TIMER_RUN_CYCLE_THRESHOLD: usize = 20;
static mut TIMER_JIFFIES: u64 = 0;

lazy_static! {
    pub static ref TIMER_LIST: SpinLock<LinkedList<Arc<Timer>>> = SpinLock::new(LinkedList::new());
}

/// 定时器要执行的函数的特征
pub trait TimerFunction: Send + Sync {
    fn run(&mut self);
}

/// WakeUpHelper函数对应的结构体
pub struct WakeUpHelper {
    pcb: &'static mut process_control_block,
}

impl WakeUpHelper {
    pub fn new(pcb: &'static mut process_control_block) -> Box<WakeUpHelper> {
        return Box::new(WakeUpHelper { pcb });
    }
}

impl TimerFunction for WakeUpHelper {
    fn run(&mut self) {
        unsafe {
            process_wakeup(self.pcb);
        }
    }
}

pub struct Timer(SpinLock<InnerTimer>);

impl Timer {
    /// @brief 创建一个定时器（单位：ms）
    ///
    /// @param timer_func 定时器需要执行的函数对应的结构体
    ///
    /// @param expire_jiffies 定时器结束时刻
    ///
    /// @return 定时器结构体
    pub fn new(timer_func: Box<dyn TimerFunction>, expire_jiffies: u64) -> Arc<Self> {
        let result: Arc<Timer> = Arc::new(Timer(SpinLock::new(InnerTimer {
            expire_jiffies,
            timer_func,
            self_ref: Weak::default(),
        })));

        result.0.lock().self_ref = Arc::downgrade(&result);

        return result;
    }

    /// @brief 将定时器插入到定时器链表中
    pub fn activate(&self) {
        let inner_guard = self.0.lock();
        let timer_list = &mut TIMER_LIST.lock();

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
            if elt.0.lock().expire_jiffies > inner_guard.expire_jiffies {
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
        self.0.lock().timer_func.run();
    }
}

/// 定时器类型
pub struct InnerTimer {
    /// 定时器结束时刻
    pub expire_jiffies: u64,
    /// 定时器需要执行的函数结构体
    pub timer_func: Box<dyn TimerFunction>,
    /// self_ref
    self_ref: Weak<Timer>,
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
        if x.is_ok() {
            return true;
        } else {
            return false;
        }
    }

    fn clear_run(&self) {
        self.running.store(false, Ordering::Release);
    }
}
impl SoftirqVec for DoTimerSoftirq {
    fn run(&self) {
        if self.set_run() == false {
            return;
        }
        // 最多只处理TIMER_RUN_CYCLE_THRESHOLD个计时器
        for _ in 0..TIMER_RUN_CYCLE_THRESHOLD {
            // kdebug!("DoTimerSoftirq run");
            let timer_list = TIMER_LIST.try_lock();
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
                let x = timer_list_front.0.try_lock();
                if x.is_err() {
                    continue;
                }
                timer_list_front_guard = Some(x.unwrap());
            }
            if timer_list_front_guard.is_none() {
                continue;
            }
            let timer_list_front_guard = timer_list_front_guard.unwrap();
            if timer_list_front_guard.expire_jiffies > unsafe { TIMER_JIFFIES as u64 } {
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

/// @brief 初始化timer模块
pub fn timer_init() {
    // FIXME 调用register_trap
    let do_timer_softirq = Arc::new(DoTimerSoftirq::new());
    softirq_vectors()
        .register_softirq(SoftirqNumber::TIMER, do_timer_softirq)
        .expect("Failed to register timer softirq");
    kdebug!("timer initiated successfully");
}

/// 计算接下来n毫秒对应的定时器时间片
pub fn next_n_ms_timer_jiffies(expire_ms: u64) -> u64 {
    return unsafe { TIMER_JIFFIES as u64 } + 1000 * (expire_ms);
}
/// 计算接下来n微秒对应的定时器时间片
pub fn next_n_us_timer_jiffies(expire_us: u64) -> u64 {
    return unsafe { TIMER_JIFFIES as u64 } + (expire_us);
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
        sched();
        return Ok(MAX_TIMEOUT);
    } else if timeout < 0 {
        kerror!("timeout can't less than 0");
        return Err(SystemError::EINVAL);
    } else {
        // 禁用中断，防止在这段期间发生调度，造成死锁
        cli();
        timeout += unsafe { TIMER_JIFFIES } as i64;
        let timer = Timer::new(WakeUpHelper::new(current_pcb()), timeout as u64);
        timer.activate();
        current_pcb().state &= (!PROC_RUNNING) as u64;
        sti();

        sched();
        let time_remaining: i64 = timeout - unsafe { TIMER_JIFFIES } as i64;
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
        match TIMER_LIST.try_lock() {
            Ok(timer_list) => {
                // kdebug!("rs_timer_get_first_expire TIMER_LIST lock successfully");
                if timer_list.is_empty() {
                    // kdebug!("timer_list is empty");
                    return Ok(0);
                } else {
                    // kdebug!("timer_list not empty");
                    return Ok(timer_list.front().unwrap().0.lock().expire_jiffies);
                }
            }
            // 加锁失败返回啥？？
            Err(_) => continue,
        }
    }
    return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
}

pub fn update_timer_jiffies(add_jiffies: u64) -> u64 {
    unsafe { TIMER_JIFFIES += add_jiffies };
    return unsafe { TIMER_JIFFIES };
}
pub fn clock() -> u64 {
    return unsafe { TIMER_JIFFIES };
}
// ====== 重构完成后请删掉extern C ======
#[no_mangle]
pub extern "C" fn rs_clock() -> u64 {
    clock()
}
#[no_mangle]
pub extern "C" fn sys_clock(_regs: *const pt_regs) -> u64 {
    clock()
}

// ====== 以下为给C提供的接口 ======
#[no_mangle]
pub extern "C" fn rs_schedule_timeout(timeout: i64) -> i64 {
    match schedule_timeout(timeout) {
        Ok(v) => {
            return v;
        }
        Err(e) => {
            kdebug!("rs_schedule_timeout run failed");
            return e.to_posix_errno() as i64;
        }
    }
}

#[no_mangle]
pub extern "C" fn rs_timer_init() {
    timer_init();
}

#[no_mangle]
pub extern "C" fn rs_timer_next_n_ms_jiffies(expire_ms: u64) -> u64 {
    return next_n_ms_timer_jiffies(expire_ms);
}

#[no_mangle]
pub extern "C" fn rs_timer_next_n_us_jiffies(expire_us: u64) -> u64 {
    return next_n_us_timer_jiffies(expire_us);
}

#[no_mangle]
pub extern "C" fn rs_timer_get_first_expire() -> i64 {
    match timer_get_first_expire() {
        Ok(v) => return v as i64,
        Err(_) => return 0,
    }
}

#[no_mangle]
pub extern "C" fn rs_update_timer_jiffies(add_jiffies: u64) -> u64 {
    return update_timer_jiffies(add_jiffies);
}
