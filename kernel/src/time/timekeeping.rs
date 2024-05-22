use alloc::sync::Arc;
use core::sync::atomic::{compiler_fence, AtomicBool, AtomicI64, AtomicUsize, Ordering};
use log::{debug, info};
use system_error::SystemError;

use crate::{
    arch::{CurrentIrqArch, CurrentTimeArch},
    exception::InterruptArch,
    libs::rwlock::{RwLock, RwLockReadGuard},
    time::{
        jiffies::{clocksource_default_clock, jiffies_init},
        timekeep::ktime_get_real_ns,
        PosixTimeSpec,
    },
};

use super::{
    clocksource::{clocksource_cyc2ns, Clocksource, CycleNum, HZ},
    syscall::PosixTimeval,
    TimeArch, NSEC_PER_SEC,
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
/// timekeeper全局变量，用于管理timekeeper模块
static mut __TIMEKEEPER: Option<Timekeeper> = None;

#[derive(Debug)]
pub struct Timekeeper {
    inner: RwLock<TimekeeperData>,

    /// 上一次更新墙上时间时的CPU周期数
    last_update_cpu_cycle: AtomicUsize,
}

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
    raw_time: PosixTimeSpec,
    wall_to_monotonic: PosixTimeSpec,
    total_sleep_time: PosixTimeSpec,
    xtime: PosixTimeSpec,
}
impl TimekeeperData {
    pub fn new() -> Self {
        Self {
            clock: None,
            shift: Default::default(),
            cycle_interval: CycleNum::new(0),
            xtime_interval: Default::default(),
            xtime_remainder: Default::default(),
            raw_interval: Default::default(),
            xtime_nsec: Default::default(),
            ntp_error: Default::default(),
            ntp_error_shift: Default::default(),
            mult: Default::default(),
            xtime: PosixTimeSpec {
                tv_nsec: 0,
                tv_sec: 0,
            },
            wall_to_monotonic: PosixTimeSpec {
                tv_nsec: 0,
                tv_sec: 0,
            },
            total_sleep_time: PosixTimeSpec {
                tv_nsec: 0,
                tv_sec: 0,
            },
            raw_time: PosixTimeSpec {
                tv_nsec: 0,
                tv_sec: 0,
            },
        }
    }
}
impl Timekeeper {
    fn new() -> Self {
        Self {
            inner: RwLock::new(TimekeeperData::new()),
            last_update_cpu_cycle: AtomicUsize::new(0),
        }
    }

    /// # 设置timekeeper的参数
    ///
    /// ## 参数
    ///
    /// * 'clock' - 指定的时钟实际类型。初始为ClocksourceJiffies
    pub fn timekeeper_setup_internals(&self, clock: Arc<dyn Clocksource>) {
        let mut timekeeper = self.inner.write_irqsave();
        // 更新clock
        let mut clock_data = clock.clocksource_data();
        clock_data.watchdog_last = clock.read();
        if clock.update_clocksource_data(clock_data).is_err() {
            debug!("timekeeper_setup_internals:update_clocksource_data run failed");
        }
        timekeeper.clock.replace(clock.clone());

        let clock_data = clock.clocksource_data();
        let mut temp = NTP_INTERVAL_LENGTH << clock_data.shift;
        let ntpinterval = temp;
        temp += (clock_data.mult / 2) as u64;
        // do div

        timekeeper.cycle_interval = CycleNum::new(temp);
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

    /// # 获取当前时钟源距离上次watchdog检测走过的纳秒数
    #[allow(dead_code)]
    pub fn tk_get_ns(&self) -> u64 {
        let timekeeper: RwLockReadGuard<'_, TimekeeperData> = self.inner.read_irqsave();
        let clock = timekeeper.clock.clone().unwrap();
        drop(timekeeper);

        let clock_now = clock.read();
        let clock_data = clock.clocksource_data();
        let clock_delta = clock_now.div(clock_data.watchdog_last).data() & clock_data.mask.bits();

        return clocksource_cyc2ns(
            CycleNum::new(clock_delta),
            clock_data.mult,
            clock_data.shift,
        );
    }

    #[inline]
    fn do_read_cpu_cycle_ns(&self) -> usize {
        let prev = self.last_update_cpu_cycle.load(Ordering::SeqCst);
        CurrentTimeArch::cycles2ns(CurrentTimeArch::get_cycles().wrapping_sub(prev))
    }

    fn mark_update_wall_time_ok(&self) {
        self.last_update_cpu_cycle
            .store(CurrentTimeArch::get_cycles(), Ordering::SeqCst);
    }
}

#[inline(always)]
pub fn timekeeper() -> &'static Timekeeper {
    let r = unsafe { __TIMEKEEPER.as_ref().unwrap() };

    return r;
}

pub fn timekeeper_init() {
    unsafe { __TIMEKEEPER = Some(Timekeeper::new()) };
}

/// # 获取1970.1.1至今的UTC时间戳(最小单位:nsec)
///
/// ## 返回值
///
/// * 'TimeSpec' - 时间戳
pub fn getnstimeofday() -> PosixTimeSpec {
    // debug!("enter getnstimeofday");

    let nsecs;
    let mut xtime: PosixTimeSpec;
    loop {
        match timekeeper().inner.try_read_irqsave() {
            None => continue,
            Some(tk) => {
                xtime = tk.xtime;
                drop(tk);
                // 提供基于cpu周期数的ns时间，以便在两次update_wall_time之间提供更好的精度
                let cpu_delta_ns = timekeeper().do_read_cpu_cycle_ns() as u64;

                // 尚未同步到xtime的时间
                let tmp_delta_ns = __ADDED_USEC.load(Ordering::SeqCst) as u64 * 1000;

                nsecs = cpu_delta_ns + tmp_delta_ns;
                // TODO 不同架构可能需要加上不同的偏移量
                break;
            }
        }
    }
    xtime.tv_nsec += nsecs as i64;
    xtime.tv_sec += xtime.tv_nsec / NSEC_PER_SEC as i64;
    xtime.tv_nsec %= NSEC_PER_SEC as i64;
    // debug!("getnstimeofday: xtime = {:?}, nsecs = {:}", xtime, nsecs);

    // TODO 将xtime和当前时间源的时间相加

    return xtime;
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

pub fn do_settimeofday64(time: PosixTimeSpec) -> Result<(), SystemError> {
    timekeeper().inner.write_irqsave().xtime = time;
    // todo: 模仿linux，实现时间误差校准。
    // https://code.dragonos.org.cn/xref/linux-6.6.21/kernel/time/timekeeping.c?fi=do_settimeofday64#1312
    return Ok(());
}

/// # 初始化timekeeping模块
#[inline(never)]
pub fn timekeeping_init() {
    info!("Initializing timekeeping module...");
    let irq_guard = unsafe { CurrentIrqArch::save_and_disable_irq() };
    timekeeper_init();

    // TODO 有ntp模块后 在此初始化ntp模块

    let clock = clocksource_default_clock();
    clock
        .enable()
        .expect("clocksource_default_clock enable failed");
    timekeeper().timekeeper_setup_internals(clock);
    // 暂时不支持其他架构平台对时间的设置 所以使用x86平台对应值初始化
    let mut timekeeper = timekeeper().inner.write_irqsave();
    timekeeper.xtime.tv_nsec = ktime_get_real_ns();

    //参考https://elixir.bootlin.com/linux/v4.4/source/kernel/time/timekeeping.c#L1251 对wtm进行初始化
    (
        timekeeper.wall_to_monotonic.tv_nsec,
        timekeeper.wall_to_monotonic.tv_sec,
    ) = (-timekeeper.xtime.tv_nsec, -timekeeper.xtime.tv_sec);

    __ADDED_USEC.store(0, Ordering::SeqCst);

    drop(irq_guard);
    drop(timekeeper);
    jiffies_init();
    info!("timekeeping_init successfully");
}

/// # 使用当前时钟源增加wall time
pub fn update_wall_time(delta_us: i64) {
    // debug!("enter update_wall_time, stack_use = {:}",stack_use);
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

    __ADDED_USEC.fetch_add(delta_us, Ordering::SeqCst);
    compiler_fence(Ordering::SeqCst);
    let mut retry = 10;

    let usec = __ADDED_USEC.load(Ordering::SeqCst);

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
                let mut timekeeper = timekeeper().inner.write_irqsave();
                timekeeper.xtime.tv_nsec = ktime_get_real_ns();
                timekeeper.xtime.tv_sec = 0;
                __ADDED_USEC.store(0, Ordering::SeqCst);

                drop(timekeeper);
                break;
            }
            retry -= 1;
        } else {
            break;
        }
    }
    timekeeper().mark_update_wall_time_ok();
    // TODO 需要检查是否更新时间源
    compiler_fence(Ordering::SeqCst);
    drop(irq_guard);
    compiler_fence(Ordering::SeqCst);
}
// TODO timekeeping_adjust
// TODO wall_to_monotic
