use core::{ ptr::null_mut};

use alloc::{
    boxed::Box,
    collections::BTreeMap,
    sync::{Arc, Weak},
};
use num_derive::FromPrimitive;
use num_traits::FromPrimitive;

use crate::{
    ipc::signal_types::SignalNumber, kdebug, libs::spinlock::{SpinLock, SpinLockGuard}, syscall::SystemError,
};

use super::TimeSpec;

pub static mut ITIMER_BTREE: *mut ItimerBtree = null_mut();
#[derive(Debug, Clone, Copy)]
pub enum SigevNotify {
    // 暂时值支持SigevSignal和SigevNone
    SigevSignal = 0,
    SigevNone = 1,
    SigevThread = 2,
    SigevThreadId = 4,
}
#[derive(FromPrimitive, ToPrimitive)]
#[repr(i32)]
pub enum ClockId {
    INVALID = -1,
    // === posix标准 ===
    ClockRealtime = 0,
    ClockMonotonic = 1,
    ClockProcessCputimeId = 2,
    ClockThreadCputimeId = 3,
    // === linux自行设计 ===
    ClockMonotonicRaw = 4,
    ClockRealtimeCoarse = 5,
    ClockMonotonicCoarse = 6,
    ClockBoottime = 7,
    ClockRealtimeAlarm = 8,
    ClockBoottimeAlarm = 9,
    ClockTai = 10,
    OtherClock = 11,
}

impl ClockId {
    // pub fn
}
// 临时支持通知机制
pub struct Sigevent {
    /// 信号
    sigev_signo: SignalNumber,
    /// 通知方式标志
    sigev_notify: SigevNotify,
}
impl Clone for Sigevent {
    fn clone(&self) -> Self {
        Self {
            sigev_signo: self.sigev_signo,
            sigev_notify: self.sigev_notify,
        }
    }
}
impl Copy for Sigevent {}
pub struct ItimerBtree {
    btree: BTreeMap<i32, Arc<KItimer>>,
}
impl ItimerBtree {
    pub fn new() -> Self {
        ItimerBtree {
            btree: BTreeMap::new(),
        }
    }
}

/// 用于支持不同的时间体系
pub trait KClock {
    fn timer_create(&self, timr: &mut SpinLockGuard<'_, InnerKItimer>);
    fn timer_set(
        &self,
        timr: &mut KItimer,
        flags: i32,
        new_setting: TimeSpec,
        old_setting: TimeSpec,
    );
    fn timer_del(&self, timr: &mut KItimer);
    fn timer_get(&self, timr: &KItimer, cur_setting: &mut TimeSpec);
}

pub struct ClockRealtime {}
impl KClock for ClockRealtime {
    fn timer_create(&self, timr: &mut SpinLockGuard<'_, InnerKItimer>) {
        common_timer_create(timr);
    }
    fn timer_del(&self, timr: &mut KItimer) {
        common_timer_del(timr)
    }
    fn timer_get(&self, timr: &KItimer, cur_setting: &mut TimeSpec) {
        common_timer_get(timr, cur_setting)
    }
    fn timer_set(
        &self,
        timr: &mut KItimer,
        flags: i32,
        new_setting: TimeSpec,
        old_setting: TimeSpec,
    ) {
        common_timer_set(timr, flags, new_setting, old_setting)
    }
}

const CLOCK_NUM: usize = 1;
pub struct KItimer(SpinLock<InnerKItimer>);
pub struct InnerKItimer {
    /// 指向上锁了的定时器的弱指针
    self_ref: Weak<KItimer>,
    /// clock类型号
    // TODO 需要宏定义为clock_t
    it_clock: ClockId,
    kclock: Option<Box<dyn KClock>>,
    /// 标记计时器处于活动状态
    it_active: i32,
    /// 信号超时时间
    it_overrun: i64,
    /// 上一次发送信号时的超时时间
    it_overrun_last: i64,
    /// 定时器在信号传递时等待被重新插入队列的标记
    it_requeue_pending: i32,
    /// 信号到达的提醒方式的标记词
    it_sigev_notify: SigevNotify,
    /// 定时器执行的时钟间隔（在linux中其类型为ktime_t，即long long）
    it_interval: i64,
    /// 需要传递的信号（如果需要的信号提醒的话）
    it_signum: SignalNumber,
}

impl KItimer {
    pub fn new() -> Arc<Self> {
        let k_itiemr = Arc::new(KItimer(SpinLock::new(InnerKItimer {
            self_ref: Weak::default(),
            it_clock: ClockId::ClockRealtime,
            it_active: Default::default(),
            it_overrun: 0,
            it_overrun_last: 0,
            it_requeue_pending: Default::default(),
            it_sigev_notify: SigevNotify::SigevSignal,
            it_interval: 0,
            it_signum: SignalNumber::SIGALRM,
            kclock: None,
        })));
        k_itiemr.0.lock().self_ref = Arc::downgrade(&k_itiemr);
        return k_itiemr;
    }
}
pub fn posix_timer_add() {}
pub fn do_timer_create(
    which_clock: i32,
    event: &Sigevent,
    create_timer_id: i32,
) -> Result<i32, SystemError> {
    match clockid_to_kclock(which_clock) {
        Err(()) => return Err(SystemError::EINVAL),
        Ok(kc) => {
            let mut k_itimer = KItimer::new();
            // TODO 调用posix_timer_add 将其加入全局红黑树
            kc.timer_create(&mut k_itimer.0.lock());
            let locked_timer = &mut k_itimer.0.lock();
            locked_timer.it_clock = FromPrimitive::from_i32(which_clock).unwrap();
            locked_timer.it_sigev_notify= event.sigev_notify;
            locked_timer.it_signum = event.sigev_signo;
            
            
            return Ok(0);
        }
    }
}
pub fn timer_create(
    which_clock: i32,
    user_timer_event: *const Sigevent,
    create_timer_id: i32,
) -> Result<i32, SystemError> {
    // ===== 请不要删掉这些注释 =====
    // if !user_timer_event.is_null() {
    //     if unsafe { verify_area(user_timer_event as u64, size_of::<Sigevent>() as u64) } == true {
    //         let event: Sigevent = Sigevent {
    //             sigev_signo: (unsafe { *user_timer_event }).sigev_signo,
    //             sigev_notify: (unsafe { *user_timer_event }).sigev_notify,
    //         };
    //         return do_timer_create(which_clock, &event, create_timer_id);
    //     } else {
    //         return Err(SystemError::EFAULT);
    //     }
    // }
    // let event = Sigevent {
    //     sigev_signo: SignalNumber::SIGALRM,
    //     sigev_notify: SigevNotify::SigevSignal,
    // };

    //TODO 暂时只支持用户手动轮询
    let event = Sigevent {
        sigev_signo: SignalNumber::INVALID,
        sigev_notify: SigevNotify::SigevNone,
    };
    return do_timer_create(which_clock, &event, create_timer_id);
}
pub fn do_timer_set(which_clock: i32, event: &Sigevent, create_timer_id: i32) {}
pub fn do_timer_get(which_clock: i32, event: &Sigevent, create_timer_id: i32) {}
pub fn do_timer_del(which_clock: i32, event: &Sigevent, create_timer_id: i32) {}

pub fn common_timer_create(timr: &mut SpinLockGuard<'_, InnerKItimer>) {}
pub fn common_timer_set(
    timr: &mut KItimer,
    flags: i32,
    new_setting: TimeSpec,
    old_setting: TimeSpec,
) {
}
pub fn common_timer_get(timr: &KItimer, cur_setting: &mut TimeSpec) {}
pub fn common_timer_del(timr: &mut KItimer) {}

pub fn clockid_to_kclock(which_clock: i32) -> Result<Box<dyn KClock>, ()> {
    if which_clock < 0 || which_clock >= CLOCK_NUM.try_into().unwrap() {
        return Err(());
    }
    let clockid = FromPrimitive::from_i32(which_clock);
    match clockid {
        Some(ClockId::ClockRealtime) => {
            let clock_realtime = Box::new(ClockRealtime {});
            return Ok(clock_realtime);
        }
        Some(ClockId::INVALID) | Some(ClockId::OtherClock) => {
            return Err(());
        }

        _ => {
            kdebug!("These clock types are temporarily not supported.");
            return Err(());
        }
    }
}
