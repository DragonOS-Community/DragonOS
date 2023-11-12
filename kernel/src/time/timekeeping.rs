use alloc::sync::Arc;
use core::sync::atomic::{compiler_fence, AtomicBool, AtomicI64, Ordering};

use crate::{
    arch::CurrentIrqArch,
    exception::InterruptArch,
    kdebug, kinfo,
    libs::rwlock::RwLock,
    time::{jiffies::clocksource_default_clock, timekeep::ktime_get_real_ns, TimeSpec},
};

use super::{
    clocksource::{clocksource_cyc2ns, Clocksource, CycleNum, HZ},
    syscall::PosixTimeval,
    NSEC_PER_SEC, USEC_PER_SEC,
};
/// NTP周期频率
pub const NTP_INTERVAL_FREQ: u64 = HZ;
/// NTP周期长度
pub const NTP_INTERVAL_LENGTH: u64 = NSEC_PER_SEC as u64 / NTP_INTERVAL_FREQ;
/// NTP转换比例
pub const NTP_SCALE_SHIFT: u32 = 32;

/// timekeeping休眠标志，false为未休眠
pub static TIMEKEEPING_SUSPENDED: AtomicBool = AtomicBool::new(false);
/// 已经递增的微秒数
static __ADDED_USEC: AtomicI64 = AtomicI64::new(0);
/// 已经递增的秒数
static __ADDED_SEC: AtomicI64 = AtomicI64::new(0);
/// timekeeper全局变量，用于管理timekeeper模块
static mut __TIMEKEEPER: Option<Timekeeper> = None;

#[derive(Debug)]
pub struct Timekeeper(RwLock<TimekeeperData>);

#[allow(dead_code)]
#[derive(Debug)]
pub struct TimekeeperData {
    /// 用于计时的当前时钟源。
    clock: Option<Arc<dyn Clocksource>>,
    /// 当前时钟源的移位值。
    shift: i32,
    /// 一个NTP间隔中的时钟周期数。
    cycle_interval: CycleNum,
    /// 一个NTP间隔中时钟移位的纳秒数。
    xtime_interval: u64,
    ///
    xtime_remainder: i64,
    /// 每个NTP间隔累积的原始纳米秒
    raw_interval: i64,
    /// 时钟移位纳米秒余数
    xtime_nsec: u64,
    /// 积累时间和ntp时间在ntp位移纳秒量上的差距
    ntp_error: i64,
    /// 用于转换时钟偏移纳秒和ntp偏移纳秒的偏移量
    ntp_error_shift: i32,
    /// NTP调整时钟乘法器
    mult: u32,
    raw_time: TimeSpec,
    wall_to_monotonic: TimeSpec,
    total_sleep_time: TimeSpec,
    xtime: TimeSpec,
}
impl TimekeeperData {
    pub fn new() -> Self {
        Self {
            clock: None,
            shift: Default::default(),
            cycle_interval: CycleNum(0),
            xtime_interval: Default::default(),
            xtime_remainder: Default::default(),
            raw_interval: Default::default(),
            xtime_nsec: Default::default(),
            ntp_error: Default::default(),
            ntp_error_shift: Default::default(),
            mult: Default::default(),
            xtime: TimeSpec {
                tv_nsec: 0,
                tv_sec: 0,
            },
            wall_to_monotonic: TimeSpec {
                tv_nsec: 0,
                tv_sec: 0,
            },
            total_sleep_time: TimeSpec {
                tv_nsec: 0,
                tv_sec: 0,
            },
            raw_time: TimeSpec {
                tv_nsec: 0,
                tv_sec: 0,
            },
        }
    }
}
impl Timekeeper {
    /// # 设置timekeeper的参数
    ///
    /// ## 参数
    ///
    /// * 'clock' - 指定的时钟实际类型。初始为ClocksourceJiffies
    pub fn timekeeper_setup_internals(&self, clock: Arc<dyn Clocksource>) {
        let mut timekeeper = self.0.write();
        // 更新clock
        let mut clock_data = clock.clocksource_data();
        clock_data.watchdog_last = clock.read();
        if clock.update_clocksource_data(clock_data).is_err() {
            kdebug!("timekeeper_setup_internals:update_clocksource_data run failed");
        }
        timekeeper.clock.replace(clock.clone());

        let clock_data = clock.clocksource_data();
        let mut temp = NTP_INTERVAL_LENGTH << clock_data.shift;
        let ntpinterval = temp;
        temp += (clock_data.mult / 2) as u64;
        // do div

        timekeeper.cycle_interval = CycleNum(temp);
        timekeeper.xtime_interval = temp * clock_data.mult as u64;
        // 这里可能存在下界溢出问题，debug模式下会报错panic
        timekeeper.xtime_remainder = (ntpinterval - timekeeper.xtime_interval) as i64;
        timekeeper.raw_interval = (timekeeper.xtime_interval >> clock_data.shift) as i64;
        timekeeper.xtime_nsec = 0;
        timekeeper.shift = clock_data.shift as i32;

        timekeeper.ntp_error = 0;
        timekeeper.ntp_error_shift = (NTP_SCALE_SHIFT - clock_data.shift) as i32;

        timekeeper.mult = clock_data.mult;
    }

    /// # 获取当前时钟源距离上次检测走过的纳秒数
    #[allow(dead_code)]
    pub fn tk_get_ns(&self) -> u64 {
        let timekeeper = self.0.read();
        let clock = timekeeper.clock.clone().unwrap();
        let clock_now = clock.read();
        let clcok_data = clock.clocksource_data();
        let clock_delta = clock_now.div(clcok_data.watchdog_last).data() & clcok_data.mask.bits();
        return clocksource_cyc2ns(CycleNum(clock_delta), clcok_data.mult, clcok_data.shift);
    }
}
pub fn timekeeper() -> &'static Timekeeper {
    let r = unsafe { __TIMEKEEPER.as_ref().unwrap() };

    return r;
}

pub fn timekeeper_init() {
    unsafe { __TIMEKEEPER = Some(Timekeeper(RwLock::new(TimekeeperData::new()))) };
}

/// # 获取1970.1.1至今的UTC时间戳(最小单位:nsec)
///
/// ## 返回值
///
/// * 'TimeSpec' - 时间戳
pub fn getnstimeofday() -> TimeSpec {
    // kdebug!("enter getnstimeofday");

    // let mut nsecs: u64 = 0;0
    let mut _xtime = TimeSpec {
        tv_nsec: 0,
        tv_sec: 0,
    };
    loop {
        match timekeeper().0.try_read() {
            None => continue,
            Some(tk) => {
                _xtime = tk.xtime;
                drop(tk);
                // nsecs = timekeeper().tk_get_ns();
                // TODO 不同架构可能需要加上不同的偏移量
                break;
            }
        }
    }
    // xtime.tv_nsec += nsecs as i64;
    let sec = __ADDED_SEC.load(Ordering::SeqCst);
    _xtime.tv_sec += sec;
    while _xtime.tv_nsec >= NSEC_PER_SEC.into() {
        _xtime.tv_nsec -= NSEC_PER_SEC as i64;
        _xtime.tv_sec += 1;
    }

    // TODO 将xtime和当前时间源的时间相加

    return _xtime;
}

/// # 获取1970.1.1至今的UTC时间戳(最小单位:usec)
///
/// ## 返回值
///
/// * 'PosixTimeval' - 时间戳
pub fn do_gettimeofday() -> PosixTimeval {
    let tp = getnstimeofday();
    return PosixTimeval {
        tv_sec: tp.tv_sec,
        tv_usec: (tp.tv_nsec / 1000) as i32,
    };
}

/// # 初始化timekeeping模块
pub fn timekeeping_init() {
    kinfo!("Initializing timekeeping module...");
    let irq_guard = unsafe { CurrentIrqArch::save_and_disable_irq() };
    timekeeper_init();

    // TODO 有ntp模块后 在此初始化ntp模块

    let clock = clocksource_default_clock();
    clock
        .enable()
        .expect("clocksource_default_clock enable failed");
    timekeeper().timekeeper_setup_internals(clock);
    // 暂时不支持其他架构平台对时间的设置 所以使用x86平台对应值初始化
    let mut timekeeper = timekeeper().0.write();
    timekeeper.xtime.tv_nsec = ktime_get_real_ns();

    // 初始化wall time到monotonic的时间
    let mut nsec = -timekeeper.xtime.tv_nsec;
    let mut sec = -timekeeper.xtime.tv_sec;
    // FIXME: 这里有个奇怪的奇怪的bug
    let num = nsec % NSEC_PER_SEC as i64;
    nsec += num * NSEC_PER_SEC as i64;
    sec -= num;
    timekeeper.wall_to_monotonic.tv_nsec = nsec;
    timekeeper.wall_to_monotonic.tv_sec = sec;

    __ADDED_USEC.store(0, Ordering::SeqCst);
    __ADDED_SEC.store(0, Ordering::SeqCst);

    drop(irq_guard);
    kinfo!("timekeeping_init successfully");
}

/// # 使用当前时钟源增加wall time
pub fn update_wall_time() {
    // kdebug!("enter update_wall_time, stack_use = {:}",stack_use);
    compiler_fence(Ordering::SeqCst);
    let irq_guard = unsafe { CurrentIrqArch::save_and_disable_irq() };
    // 如果在休眠那就不更新
    if TIMEKEEPING_SUSPENDED.load(Ordering::SeqCst) {
        return;
    }

    // ===== 请不要删除这些注释 =====
    // let clock = timekeeper.clock.clone().unwrap();
    // let clock_data = clock.clocksource_data();
    // let offset = (clock.read().div(clock_data.watchdog_last).data()) & clock_data.mask.bits();

    // timekeeper.xtime_nsec = (timekeeper.xtime.tv_nsec as u64) << timekeeper.shift;
    // // TODO 当有ntp模块之后 需要将timekeep与ntp进行同步并检查
    // timekeeper.xtime.tv_nsec = ((timekeeper.xtime_nsec as i64) >> timekeeper.shift) + 1;
    // timekeeper.xtime_nsec -= (timekeeper.xtime.tv_nsec as u64) << timekeeper.shift;

    // timekeeper.xtime.tv_nsec += offset as i64;
    // while unlikely(timekeeper.xtime.tv_nsec >= NSEC_PER_SEC.into()) {
    //     timekeeper.xtime.tv_nsec -= NSEC_PER_SEC as i64;
    //     timekeeper.xtime.tv_sec += 1;
    //     // TODO 需要处理闰秒
    // }
    // ================
    compiler_fence(Ordering::SeqCst);

    // !!! todo: 这里是硬编码了HPET的500us中断，需要修改
    __ADDED_USEC.fetch_add(500, Ordering::SeqCst);
    compiler_fence(Ordering::SeqCst);
    let mut retry = 10;

    let usec = __ADDED_USEC.load(Ordering::SeqCst);
    if usec % USEC_PER_SEC as i64 == 0 {
        compiler_fence(Ordering::SeqCst);

        __ADDED_SEC.fetch_add(1, Ordering::SeqCst);
        compiler_fence(Ordering::SeqCst);
    }
    // 一分钟同步一次
    loop {
        if (usec & !((1 << 26) - 1)) != 0 {
            if __ADDED_USEC
                .compare_exchange(usec, 0, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
                || retry == 0
            {
                // 同步时间
                // 我感觉这里会出问题：多个读者不退出的话，写者就无法写入
                // 然后这里会超时，导致在中断返回之后，会不断的进入这个中断，最终爆栈。
                let mut timekeeper = timekeeper().0.write_irqsave();
                timekeeper.xtime.tv_nsec = ktime_get_real_ns();
                timekeeper.xtime.tv_sec = 0;
                __ADDED_SEC.store(0, Ordering::SeqCst);
                drop(timekeeper);
                break;
            }
            retry -= 1;
        } else {
            break;
        }
    }
    // TODO 需要检查是否更新时间源
    compiler_fence(Ordering::SeqCst);
    drop(irq_guard);
    compiler_fence(Ordering::SeqCst);
}
// TODO timekeeping_adjust
// TODO wall_to_monotic

// ========= 以下为对C的接口 =========
#[no_mangle]
pub extern "C" fn rs_timekeeping_init() {
    timekeeping_init();
}
