use core::{
    fmt,
    intrinsics::unlikely,
    ops::{self, Sub},
};

use crate::arch::CurrentTimeArch;

use self::timekeep::ktime_get_real_ns;

pub mod clocksource;
pub mod jiffies;
pub mod sleep;
pub mod syscall;
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

/// 表示时间的结构体，符合POSIX标准。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(C)]
pub struct TimeSpec {
    pub tv_sec: i64,
    pub tv_nsec: i64,
}

impl TimeSpec {
    #[allow(dead_code)]
    pub fn new(sec: i64, nsec: i64) -> TimeSpec {
        return TimeSpec {
            tv_sec: sec,
            tv_nsec: nsec,
        };
    }

    /// 获取当前时间
    pub fn now() -> Self {
        #[cfg(target_arch = "x86_64")]
        {
            use crate::arch::driver::tsc::TSCManager;
            let khz = TSCManager::cpu_khz();
            if unlikely(khz == 0) {
                return TimeSpec::default();
            } else {
                return Self::from(Duration::from_millis(
                    CurrentTimeArch::get_cycles() as u64 / khz,
                ));
            }
        }

        #[cfg(target_arch = "riscv64")]
        {
            return TimeSpec::new(0, 0);
        }
    }
}

impl Sub for TimeSpec {
    type Output = Duration;
    fn sub(self, rhs: Self) -> Self::Output {
        let sec = self.tv_sec.checked_sub(rhs.tv_sec).unwrap_or(0);
        let nsec = self.tv_nsec.checked_sub(rhs.tv_nsec).unwrap_or(0);
        Duration::from_micros((sec * 1000000 + nsec / 1000) as u64)
    }
}

impl From<Duration> for TimeSpec {
    fn from(dur: Duration) -> Self {
        TimeSpec {
            tv_sec: dur.total_micros() as i64 / 1000000,
            tv_nsec: (dur.total_micros() as i64 % 1000000) * 1000,
        }
    }
}

impl From<TimeSpec> for Duration {
    fn from(val: TimeSpec) -> Self {
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
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub struct Instant {
    micros: i64,
}

#[allow(dead_code)]
impl Instant {
    pub const ZERO: Instant = Instant::from_micros_const(0);

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
        Self::from_micros(ktime_get_real_ns() / 1000)
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
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
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
}
