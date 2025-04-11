use core::{
    fmt,
    intrinsics::unlikely,
    ops::{self, Sub},
};

use crate::arch::CurrentTimeArch;
use crate::time::syscall::PosixTimeval;

use self::timekeeping::getnstimeofday;

pub mod clocksource;
pub mod jiffies;
pub mod sleep;
pub mod syscall;
pub mod tick_common;
pub mod timeconv;
pub mod timekeep;
pub mod timekeeping;
pub mod timer;

/* Time structures. (Partitially taken from smoltcp)

The `time` module contains structures used to represent both
absolute and relative time.

 - [Instant] is used to represent absolute time.
 - [Duration] is used to represent relative time.

[Instant]: struct.Instant.html
[Duration]: struct.Duration.html
*/
#[allow(dead_code)]
pub const MSEC_PER_SEC: u32 = 1000;
#[allow(dead_code)]
pub const USEC_PER_MSEC: u32 = 1000;
#[allow(dead_code)]
pub const NSEC_PER_USEC: u32 = 1000;
#[allow(dead_code)]
pub const NSEC_PER_MSEC: u32 = 1000000;
#[allow(dead_code)]
pub const USEC_PER_SEC: u32 = 1000000;
#[allow(dead_code)]
pub const NSEC_PER_SEC: u32 = 1000000000;
#[allow(dead_code)]
pub const FSEC_PER_SEC: u64 = 1000000000000000;

/// The clock frequency of the i8253/i8254 PIT
pub const PIT_TICK_RATE: u64 = 1193182;

/// 表示时间的结构体，符合POSIX标准。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(C)]
pub struct PosixTimeSpec {
    pub tv_sec: i64,
    pub tv_nsec: i64,
}

impl PosixTimeSpec {
    #[allow(dead_code)]
    pub fn new(sec: i64, nsec: i64) -> PosixTimeSpec {
        return PosixTimeSpec {
            tv_sec: sec,
            tv_nsec: nsec,
        };
    }

    /// 获取当前时间
    #[inline(always)]
    pub fn now() -> Self {
        getnstimeofday()
    }

    /// 获取当前CPU时间(使用CPU时钟周期计算，会存在回绕问题)
    pub fn now_cpu_time() -> Self {
        #[cfg(target_arch = "x86_64")]
        {
            use crate::arch::driver::tsc::TSCManager;
            let khz = TSCManager::cpu_khz();
            if unlikely(khz == 0) {
                return PosixTimeSpec::default();
            } else {
                return Self::from(Duration::from_millis(
                    CurrentTimeArch::get_cycles() as u64 / khz,
                ));
            }
        }

        #[cfg(any(target_arch = "riscv64", target_arch = "loongarch64"))]
        {
            return PosixTimeSpec::new(0, 0);
        }
    }

    /// 换算成纳秒
    pub fn total_nanos(&self) -> i64 {
        self.tv_sec * 1000000000 + self.tv_nsec
    }
}

impl Sub for PosixTimeSpec {
    type Output = Duration;
    fn sub(self, rhs: Self) -> Self::Output {
        let sec = self.tv_sec.checked_sub(rhs.tv_sec).unwrap_or(0);
        let nsec = self.tv_nsec.checked_sub(rhs.tv_nsec).unwrap_or(0);
        Duration::from_micros((sec * 1000000 + nsec / 1000) as u64)
    }
}

impl From<Duration> for PosixTimeSpec {
    fn from(dur: Duration) -> Self {
        PosixTimeSpec {
            tv_sec: dur.total_micros() as i64 / 1000000,
            tv_nsec: (dur.total_micros() as i64 % 1000000) * 1000,
        }
    }
}

impl From<PosixTimeval> for PosixTimeSpec {
    fn from(value: PosixTimeval) -> Self {
        PosixTimeSpec {
            tv_sec: value.tv_sec,
            tv_nsec: value.tv_usec as i64 * 1000,
        }
    }
}

impl From<PosixTimeSpec> for Duration {
    fn from(val: PosixTimeSpec) -> Self {
        Duration::from_micros(val.tv_sec as u64 * 1000000 + val.tv_nsec as u64 / 1000)
    }
}

/// A representation of an absolute time value.
///
/// The `Instant` type is a wrapper around a `i64` value that
/// represents a number of microseconds, monotonically increasing
/// since an arbitrary moment in time, such as system startup.
///
/// * A value of `0` is inherently arbitrary.
/// * A value less than `0` indicates a time before the starting
///   point.
#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Instant {
    micros: i64,
}

#[allow(dead_code)]
impl Instant {
    pub const ZERO: Instant = Instant::from_micros_const(0);

    /// mktime64 - 将日期转换为秒。
    ///
    /// ## 参数
    ///
    /// - year0: 要转换的年份
    /// - mon0: 要转换的月份
    /// - day: 要转换的天
    /// - hour: 要转换的小时
    /// - min: 要转换的分钟
    /// - sec: 要转换的秒
    ///
    /// 将公历日期转换为1970-01-01 00:00:00以来的秒数。
    /// 假设输入为正常的日期格式，即1980-12-31 23:59:59 => 年份=1980, 月=12, 日=31, 时=23, 分=59, 秒=59。
    ///
    /// [For the Julian calendar（俄罗斯在1917年之前使用，英国及其殖民地在大西洋1752年之前使用，
    /// 其他地方在1582年之前使用，某些社区仍然在使用）省略-year/100+year/400项，
    /// 并在结果上加10。]
    ///
    /// 这个算法最初由高斯（我认为是）发表。
    ///
    /// 要表示闰秒，可以通过将sec设为60（在ISO 8601允许）来调用此函数。
    /// 闰秒与随后的秒一样处理，因为它们在UNIX时间中不存在。
    ///
    /// 支持将午夜作为当日末尾的24:00:00编码 - 即明天的午夜（在ISO 8601允许）。
    ///
    /// ## 返回
    ///
    /// 返回：给定输入日期自1970-01-01 00:00:00以来的秒数
    pub fn mktime64(year0: u32, mon0: u32, day: u32, hour: u32, min: u32, sec: u32) -> Self {
        let mut mon: i64 = mon0.into();
        let mut year: u64 = year0.into();
        let day: u64 = day.into();
        let hour: u64 = hour.into();
        let min: u64 = min.into();
        let sec: u64 = sec.into();

        mon -= 2;
        /* 1..12 -> 11,12,1..10 */
        if mon <= 0 {
            /* Puts Feb last since it has leap day */
            mon += 12;
            year -= 1;
        }
        let mon = mon as u64;

        let secs = ((((year / 4 - year / 100 + year / 400 + 367 * mon / 12 + day) + year * 365
            - 719499)
            * 24 + hour) /* now have hours - midnight tomorrow handled here */
            * 60 + min)/* now have minutes */
            * 60
            + sec; /* finally seconds */

        Self::from_secs(secs as i64)
    }

    /// Create a new `Instant` from a number of microseconds.
    pub fn from_micros<T: Into<i64>>(micros: T) -> Instant {
        Instant {
            micros: micros.into(),
        }
    }

    pub const fn from_micros_const(micros: i64) -> Instant {
        Instant { micros }
    }

    /// Create a new `Instant` from a number of milliseconds.
    pub fn from_millis<T: Into<i64>>(millis: T) -> Instant {
        Instant {
            micros: millis.into() * 1000,
        }
    }

    /// Create a new `Instant` from a number of milliseconds.
    pub const fn from_millis_const(millis: i64) -> Instant {
        Instant {
            micros: millis * 1000,
        }
    }

    /// Create a new `Instant` from a number of seconds.
    pub fn from_secs<T: Into<i64>>(secs: T) -> Instant {
        Instant {
            micros: secs.into() * 1000000,
        }
    }

    /// Create a new `Instant` from the current time
    pub fn now() -> Instant {
        let tm = getnstimeofday();
        Self::from_micros(tm.tv_sec * 1000000 + tm.tv_nsec / 1000)
    }

    /// The fractional number of milliseconds that have passed
    /// since the beginning of time.
    pub const fn millis(&self) -> i64 {
        self.micros % 1000000 / 1000
    }

    /// The fractional number of microseconds that have passed
    /// since the beginning of time.
    pub const fn micros(&self) -> i64 {
        self.micros % 1000000
    }

    /// The number of whole seconds that have passed since the
    /// beginning of time.
    pub const fn secs(&self) -> i64 {
        self.micros / 1000000
    }

    /// The total number of milliseconds that have passed since
    /// the beginning of time.
    pub const fn total_millis(&self) -> i64 {
        self.micros / 1000
    }
    /// The total number of milliseconds that have passed since
    /// the beginning of time.
    pub const fn total_micros(&self) -> i64 {
        self.micros
    }

    /// Returns the duration between this instant and another one.
    ///
    /// # Arguments
    ///
    /// * `earlier` - The earlier instant to calculate the duration since.
    ///
    /// # Returns
    ///
    /// An `Option<Duration>` representing the duration between this instant and the earlier one.
    /// If the earlier instant is later than this one, it returns `None`.
    pub fn duration_since(&self, earlier: Instant) -> Option<Duration> {
        if earlier.micros > self.micros {
            return None;
        }
        let micros_diff = self.micros - earlier.micros;
        Some(Duration::from_micros(micros_diff as u64))
    }

    /// Saturating subtraction. Computes `self - other`, returning [`Instant::ZERO`] if the result would be negative.
    ///
    /// # Arguments
    ///
    /// * `other` - The `Instant` to subtract from `self`.
    ///
    /// # Returns
    ///
    /// The duration between `self` and `other`, or [`Instant::ZERO`] if `other` is later than `self`.
    pub fn saturating_sub(self, other: Instant) -> Duration {
        if self.micros >= other.micros {
            Duration::from_micros((self.micros - other.micros) as u64)
        } else {
            Duration::ZERO
        }
    }
}

impl fmt::Display for Instant {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}.{:0>3}s", self.secs(), self.millis())
    }
}

impl ops::Add<Duration> for Instant {
    type Output = Instant;

    fn add(self, rhs: Duration) -> Instant {
        Instant::from_micros(self.micros + rhs.total_micros() as i64)
    }
}

impl ops::AddAssign<Duration> for Instant {
    fn add_assign(&mut self, rhs: Duration) {
        self.micros += rhs.total_micros() as i64;
    }
}

impl ops::Sub<Duration> for Instant {
    type Output = Instant;

    fn sub(self, rhs: Duration) -> Instant {
        Instant::from_micros(self.micros - rhs.total_micros() as i64)
    }
}

impl ops::SubAssign<Duration> for Instant {
    fn sub_assign(&mut self, rhs: Duration) {
        self.micros -= rhs.total_micros() as i64;
    }
}

impl ops::Sub<Instant> for Instant {
    type Output = Duration;

    fn sub(self, rhs: Instant) -> Duration {
        Duration::from_micros((self.micros - rhs.micros).unsigned_abs())
    }
}

/// A relative amount of time.
#[derive(Debug, Default, Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Duration {
    micros: u64,
}

impl Duration {
    pub const ZERO: Duration = Duration::from_micros(0);
    /// Create a new `Duration` from a number of microseconds.
    pub const fn from_micros(micros: u64) -> Duration {
        Duration { micros }
    }

    /// Create a new `Duration` from a number of milliseconds.
    pub const fn from_millis(millis: u64) -> Duration {
        Duration {
            micros: millis * 1000,
        }
    }

    /// Create a new `Instant` from a number of seconds.
    pub const fn from_secs(secs: u64) -> Duration {
        Duration {
            micros: secs * 1000000,
        }
    }

    /// The fractional number of milliseconds in this `Duration`.
    pub const fn millis(&self) -> u64 {
        self.micros / 1000 % 1000
    }

    /// The fractional number of milliseconds in this `Duration`.
    pub const fn micros(&self) -> u64 {
        self.micros % 1000000
    }

    /// The number of whole seconds in this `Duration`.
    pub const fn secs(&self) -> u64 {
        self.micros / 1000000
    }

    /// The total number of milliseconds in this `Duration`.
    pub const fn total_millis(&self) -> u64 {
        self.micros / 1000
    }

    /// The total number of microseconds in this `Duration`.
    pub const fn total_micros(&self) -> u64 {
        self.micros
    }
}

impl fmt::Display for Duration {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}.{:03}s", self.secs(), self.millis())
    }
}

impl ops::Add<Duration> for Duration {
    type Output = Duration;

    fn add(self, rhs: Duration) -> Duration {
        Duration::from_micros(self.micros + rhs.total_micros())
    }
}

impl ops::AddAssign<Duration> for Duration {
    fn add_assign(&mut self, rhs: Duration) {
        self.micros += rhs.total_micros();
    }
}

impl ops::Sub<Duration> for Duration {
    type Output = Duration;

    fn sub(self, rhs: Duration) -> Duration {
        Duration::from_micros(
            self.micros
                .checked_sub(rhs.total_micros())
                .expect("overflow when subtracting durations"),
        )
    }
}

impl ops::SubAssign<Duration> for Duration {
    fn sub_assign(&mut self, rhs: Duration) {
        self.micros = self
            .micros
            .checked_sub(rhs.total_micros())
            .expect("overflow when subtracting durations");
    }
}

impl ops::Mul<u32> for Duration {
    type Output = Duration;

    fn mul(self, rhs: u32) -> Duration {
        Duration::from_micros(self.micros * rhs as u64)
    }
}

impl ops::MulAssign<u32> for Duration {
    fn mul_assign(&mut self, rhs: u32) {
        self.micros *= rhs as u64;
    }
}

impl ops::Div<u32> for Duration {
    type Output = Duration;

    fn div(self, rhs: u32) -> Duration {
        Duration::from_micros(self.micros / rhs as u64)
    }
}

impl ops::DivAssign<u32> for Duration {
    fn div_assign(&mut self, rhs: u32) {
        self.micros /= rhs as u64;
    }
}

impl ops::Shl<u32> for Duration {
    type Output = Duration;

    fn shl(self, rhs: u32) -> Duration {
        Duration::from_micros(self.micros << rhs)
    }
}

impl ops::ShlAssign<u32> for Duration {
    fn shl_assign(&mut self, rhs: u32) {
        self.micros <<= rhs;
    }
}

impl ops::Shr<u32> for Duration {
    type Output = Duration;

    fn shr(self, rhs: u32) -> Duration {
        Duration::from_micros(self.micros >> rhs)
    }
}

impl ops::ShrAssign<u32> for Duration {
    fn shr_assign(&mut self, rhs: u32) {
        self.micros >>= rhs;
    }
}

impl From<::core::time::Duration> for Duration {
    fn from(other: ::core::time::Duration) -> Duration {
        Duration::from_micros(other.as_secs() * 1000000 + other.subsec_micros() as u64)
    }
}

impl From<Duration> for ::core::time::Duration {
    fn from(val: Duration) -> Self {
        ::core::time::Duration::from_micros(val.total_micros())
    }
}

/// 支持与smoltcp的时间转换
impl From<smoltcp::time::Instant> for Instant {
    fn from(val: smoltcp::time::Instant) -> Self {
        Instant::from_micros(val.micros())
    }
}

impl From<Instant> for smoltcp::time::Instant {
    fn from(val: Instant) -> Self {
        smoltcp::time::Instant::from_millis(val.millis())
    }
}

/// 支持与smoltcp的时间转换
impl From<smoltcp::time::Duration> for Duration {
    fn from(val: smoltcp::time::Duration) -> Self {
        Duration::from_micros(val.micros())
    }
}

impl From<Duration> for smoltcp::time::Duration {
    fn from(val: Duration) -> Self {
        smoltcp::time::Duration::from_millis(val.millis())
    }
}

pub trait TimeArch {
    /// Get CPU cycles (Read from register)
    fn get_cycles() -> usize;

    /// Calculate expire cycles
    ///
    /// # Arguments
    ///
    /// - `ns` - The time to expire in nanoseconds
    ///
    /// # Returns
    ///
    /// The expire cycles
    fn cal_expire_cycles(ns: usize) -> usize;

    /// 将CPU的时钟周期数转换为纳秒
    fn cycles2ns(cycles: usize) -> usize;
}
