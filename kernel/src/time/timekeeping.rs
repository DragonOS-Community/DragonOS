use alloc::sync::Arc;
use core::intrinsics::{likely, unlikely};
use core::sync::atomic::{compiler_fence, AtomicBool, Ordering};
use log::{debug, info, warn};
use system_error::SystemError;

use crate::{
    arch::CurrentIrqArch,
    exception::InterruptArch,
    libs::rwlock::RwLock,
    time::{
        jiffies::{clocksource_default_clock, jiffies_init},
        timekeep::ktime_get_real_ns,
        PosixTimeSpec,
    },
};

use super::timekeep::{ktime_t, timespec_to_ktime};
use super::{
    clocksource::{clocksource_cyc2ns, Clocksource, CycleNum, HZ},
    syscall::PosixTimeval,
    NSEC_PER_SEC,
};
/// NTP周期频率
pub const NTP_INTERVAL_FREQ: u64 = HZ;
/// NTP周期长度
pub const NTP_INTERVAL_LENGTH: u64 = NSEC_PER_SEC as u64 / NTP_INTERVAL_FREQ;
/// NTP转换比例
pub const NTP_SCALE_SHIFT: u32 = 32;

/// timekeeping休眠标志，false为未休眠
pub static TIMEKEEPING_SUSPENDED: AtomicBool = AtomicBool::new(false);
/// timekeeper全局变量，用于管理timekeeper模块
static mut __TIMEKEEPER: Option<Timekeeper> = None;

#[derive(Debug)]
pub struct Timekeeper {
    inner: RwLock<TimekeeperData>,
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
    /// 单调时间和实时时间的偏移量
    real_time_offset: ktime_t,
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
            real_time_offset: 0,
        }
    }
}
impl Timekeeper {
    fn new() -> Self {
        Self {
            inner: RwLock::new(TimekeeperData::new()),
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
        clock_data.cycle_last = clock.read();
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

    pub fn timekeeping_get_ns(&self) -> i64 {
        let timekeeper = self.inner.read_irqsave();
        let clock = timekeeper.clock.clone().unwrap();

        let cycle_now = clock.read();
        let clock_data = clock.clocksource_data();
        let cycle_delta = (cycle_now.div(clock_data.cycle_last)).data() & clock_data.mask.bits();

        return clocksource_cyc2ns(
            CycleNum::new(cycle_delta),
            timekeeper.mult,
            timekeeper.shift as u32,
        ) as i64;
    }

    /// # 处理大幅度调整
    pub fn timekeeping_bigadjust(&self, error: i64, interval: i64, offset: i64) -> (i64, i64, i32) {
        let mut error = error;
        let mut interval = interval;
        let mut offset = offset;

        // TODO: 计算look_head并调整ntp误差

        let tmp = interval;
        let mut mult = 1;
        let mut adj = 0;
        if error < 0 {
            error = -error;
            interval = -interval;
            offset = -offset;
            mult = -1;
        }
        while error > tmp {
            adj += 1;
            error >>= 1;
        }

        interval <<= adj;
        offset <<= adj;
        mult <<= adj;

        return (interval, offset, mult);
    }

    /// # 调整时钟的mult减少ntp_error
    pub fn timekeeping_adjust(&self, offset: i64) -> i64 {
        let mut timekeeper = self.inner.write_irqsave();
        let mut interval = timekeeper.cycle_interval.data() as i64;
        let mut offset = offset;
        let adj: i32;

        // 计算误差
        let mut error = timekeeper.ntp_error >> (timekeeper.ntp_error_shift - 1);

        // 误差超过一个interval，就要进行调整
        if error >= 0 {
            if error > interval {
                error >>= 2;
                if likely(error <= interval) {
                    adj = 1;
                } else {
                    (interval, offset, adj) = self.timekeeping_bigadjust(error, interval, offset);
                }
            } else {
                // 不需要校准
                return offset;
            }
        } else if -error > interval {
            if likely(-error <= interval) {
                adj = -1;
                interval = -interval;
                offset = -offset;
            } else {
                (interval, offset, adj) = self.timekeeping_bigadjust(error, interval, offset);
            }
        } else {
            // 不需要校准
            return offset;
        }

        // 检查最大调整值，确保调整值不会超过时钟源允许的最大值
        let clock_data = timekeeper.clock.clone().unwrap().clocksource_data();
        if unlikely(
            clock_data.maxadj != 0
                && (timekeeper.mult as i32 + adj
                    > clock_data.mult as i32 + clock_data.maxadj as i32),
        ) {
            warn!(
                "Adjusting {:?} more than ({} vs {})",
                clock_data.name,
                timekeeper.mult as i32 + adj,
                clock_data.mult as i32 + clock_data.maxadj as i32
            );
        }

        if error > 0 {
            timekeeper.mult += adj as u32;
            timekeeper.xtime_interval += interval as u64;
            timekeeper.xtime_nsec -= offset as u64;
        } else {
            timekeeper.mult -= adj as u32;
            timekeeper.xtime_interval -= interval as u64;
            timekeeper.xtime_nsec += offset as u64;
        }
        timekeeper.ntp_error -= (interval - offset) << timekeeper.ntp_error_shift;

        return offset;
    }
    /// # 用于累积时间间隔，并将其转换为纳秒时间
    pub fn logarithmic_accumulation(&self, offset: u64, shift: i32) -> u64 {
        let mut timekeeper = self.inner.write_irqsave();
        let clock = timekeeper.clock.clone().unwrap();
        let clock_data = clock.clocksource_data();
        let nsecps = (NSEC_PER_SEC as u64) << timekeeper.shift;
        let mut offset = offset;

        // 检查offset是否小于一个NTP周期间隔
        if offset < timekeeper.cycle_interval.data() << shift {
            return offset;
        }

        // 累积一个移位的interval
        offset -= timekeeper.cycle_interval.data() << shift;
        clock_data
            .cycle_last
            .add(CycleNum::new(timekeeper.cycle_interval.data() << shift));
        if clock.update_clocksource_data(clock_data).is_err() {
            debug!("logarithmic_accumulation:update_clocksource_data run failed");
        }
        timekeeper.clock.replace(clock.clone());

        // 更新xime_nsec
        timekeeper.xtime_nsec += timekeeper.xtime_interval << shift;
        while timekeeper.xtime_nsec >= nsecps {
            timekeeper.xtime_nsec -= nsecps;
            timekeeper.xtime.tv_sec += 1;
            // TODO: 处理闰秒
        }

        // TODO：更新raw_time

        // TODO：计算ntp_error

        return offset;
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

                nsecs = timekeeper().timekeeping_get_ns();

                // TODO 不同架构可能需要加上不同的偏移量
                break;
            }
        }
    }
    xtime.tv_nsec += nsecs;
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

    drop(irq_guard);
    drop(timekeeper);
    jiffies_init();
    info!("timekeeping_init successfully");
}

/// # 使用当前时钟源增加wall time
/// 参考：https://code.dragonos.org.cn/xref/linux-3.4.99/kernel/time/timekeeping.c#1041
pub fn update_wall_time() {
    // debug!("enter update_wall_time, stack_use = {:}",stack_use);
    compiler_fence(Ordering::SeqCst);
    let irq_guard = unsafe { CurrentIrqArch::save_and_disable_irq() };
    // 如果在休眠那就不更新
    if TIMEKEEPING_SUSPENDED.load(Ordering::SeqCst) {
        return;
    }

    let mut tk = timekeeper().inner.write_irqsave();
    // 获取当前时钟源
    let clock = tk.clock.clone().unwrap();
    let clock_data = clock.clocksource_data();
    // 计算从上一次更新周期以来经过的时钟周期数
    let mut offset = (clock.read().div(clock_data.cycle_last).data()) & clock_data.mask.bits();
    // 检查offset是否达到了一个NTP周期间隔
    if offset < tk.cycle_interval.data() {
        return;
    }

    // 将纳秒部分转换为更高精度的格式
    tk.xtime_nsec = (tk.xtime.tv_nsec as u64) << tk.shift;

    let mut shift = (offset.ilog2() - tk.cycle_interval.data().ilog2()) as i32;
    shift = shift.max(0);
    // let max_shift = (64 - (ntp_tick_length().ilog2()+1)) - 1;
    // shift = min(shift, max_shift)
    while offset >= tk.cycle_interval.data() {
        offset = timekeeper().logarithmic_accumulation(offset, shift);
        if offset < tk.cycle_interval.data() << shift {
            shift -= 1;
        }
    }

    timekeeper().timekeeping_adjust(offset as i64);

    // 处理xtime_nsec下溢问题，并对NTP误差进行调整
    if unlikely((tk.xtime_nsec as i64) < 0) {
        let neg = -(tk.xtime_nsec as i64);
        tk.xtime_nsec = 0;
        tk.ntp_error += neg << tk.ntp_error_shift;
    }

    // 将纳秒部分舍入后存储在xtime.tv_nsec中
    tk.xtime.tv_nsec = ((tk.xtime_nsec as i64) >> tk.shift) + 1;
    tk.xtime_nsec -= (tk.xtime.tv_nsec as u64) << tk.shift;

    // 确保经过舍入后的xtime.tv_nsec不会大于NSEC_PER_SEC，并在超过1秒的情况下进行适当的调整
    if unlikely(tk.xtime.tv_nsec >= NSEC_PER_SEC.into()) {
        tk.xtime.tv_nsec -= NSEC_PER_SEC as i64;
        tk.xtime.tv_sec += 1;
        // TODO: 处理闰秒
    }

    // 更新时间的相关信息
    timekeeping_update();

    compiler_fence(Ordering::SeqCst);
    drop(irq_guard);
    compiler_fence(Ordering::SeqCst);
}
// TODO wall_to_monotic

/// 参考：https://code.dragonos.org.cn/xref/linux-3.4.99/kernel/time/timekeeping.c#190
pub fn timekeeping_update() {
    // TODO：如果clearntp为true，则会清除NTP错误并调用ntp_clear()

    // 更新实时时钟偏移量，用于跟踪硬件时钟与系统时间的差异，以便进行时间校正
    update_rt_offset();
}

/// # 更新实时偏移量(墙上之间与单调时间的差值)
pub fn update_rt_offset() {
    let mut timekeeper = timekeeper().inner.write_irqsave();
    let ts = PosixTimeSpec::new(
        -timekeeper.wall_to_monotonic.tv_sec,
        -timekeeper.wall_to_monotonic.tv_nsec,
    );
    timekeeper.real_time_offset = timespec_to_ktime(ts);
}
