use core::ffi::{c_int, c_longlong};
use num_traits::FromPrimitive;
use system_error::SystemError;

use crate::syscall::Syscall;

#[cfg(target_arch = "x86_64")]
mod sys_alarm;
mod sys_clock_gettime;
mod sys_clock_nanosleep;
mod sys_getitimer;
mod sys_gettimeofday;
mod sys_nanosleep;
mod sys_setitimer;
mod sys_timer_create;
mod sys_timer_delete;
mod sys_timer_getoverrun;
mod sys_timer_gettime;
mod sys_timer_settime;

pub type PosixTimeT = c_longlong;
pub type PosixSusecondsT = c_int;

#[repr(C)]
#[derive(Default, Debug, Copy, Clone)]
pub struct PosixTimeval {
    pub tv_sec: PosixTimeT,
    pub tv_usec: PosixSusecondsT,
}

impl PosixTimeval {
    /// 从内核统一使用的 u64 纳秒创建一个 PosixTimeval 实例
    pub fn from_ns(ns: u64) -> Self {
        Self {
            tv_sec: (ns / 1_000_000_000) as PosixTimeT,
            tv_usec: ((ns % 1_000_000_000) / 1000) as PosixSusecondsT,
        }
    }

    /// 将当前的 PosixTimeval 精确转换为内核统一使用的 u64 纳秒
    pub fn to_ns(self) -> u64 {
        if self.tv_usec < 0 || self.tv_usec >= 1_000_000 as PosixSusecondsT {
            (self.tv_sec as u64).saturating_mul(1_000_000_000)
        } else {
            let sec_ns = (self.tv_sec as u64).saturating_mul(1_000_000_000);
            let usec_ns = (self.tv_usec as u64).saturating_mul(1000);
            sec_ns.saturating_add(usec_ns)
        }
    }
}

#[repr(C)]
#[derive(Default, Debug, Copy, Clone)]
pub struct Itimerval {
    pub it_interval: PosixTimeval,
    pub it_value: PosixTimeval,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, FromPrimitive)]
pub enum ItimerType {
    /// 真实时间定时器，基于时钟时间。触发 SIGALRM
    Real = 0,
    /// 虚拟时间定时器，仅计算进程在用户态下的CPU时间。触发 SIGVTALRM
    Virtual = 1,
    /// 性能分析定时器，计算进程在用户态和内核态下的总CPU时间。触发 SIGPROF
    Prof = 2,
}

impl TryFrom<i32> for ItimerType {
    type Error = SystemError;

    fn try_from(value: i32) -> Result<Self, Self::Error> {
        <Self as num_traits::FromPrimitive>::from_i32(value).ok_or(SystemError::EINVAL)
    }
}

#[repr(C)]
#[derive(Default, Debug, Copy, Clone)]
/// 当前时区信息
pub struct PosixTimeZone {
    /// 格林尼治相对于当前时区相差的分钟数
    pub tz_minuteswest: c_int,
    /// DST矫正时差
    pub tz_dsttime: c_int,
}

/// 系统时区 暂时写定为东八区
pub const SYS_TIMEZONE: PosixTimeZone = PosixTimeZone {
    tz_minuteswest: -480,
    tz_dsttime: 0,
};

/// The IDs of the various system clocks (for POSIX.1b interval timers):
#[derive(Debug, Copy, Clone, PartialEq, Eq, FromPrimitive)]
pub enum PosixClockID {
    Realtime = 0,
    Monotonic = 1,
    ProcessCPUTimeID = 2,
    ThreadCPUTimeID = 3,
    MonotonicRaw = 4,
    RealtimeCoarse = 5,
    MonotonicCoarse = 6,
    Boottime = 7,
    RealtimeAlarm = 8,
    BoottimeAlarm = 9,
}

impl TryFrom<i32> for PosixClockID {
    type Error = SystemError;

    fn try_from(value: i32) -> Result<Self, Self::Error> {
        <Self as FromPrimitive>::from_i32(value).ok_or(SystemError::EINVAL)
    }
}

impl Syscall {
    /// 获取cpu时间
    ///
    /// todo: 该系统调用与Linux不一致，将来需要删除该系统调用！！！ 删的时候记得改C版本的libc
    pub fn clock() -> Result<usize, SystemError> {
        return Ok(super::timer::clock() as usize);
    }
}
