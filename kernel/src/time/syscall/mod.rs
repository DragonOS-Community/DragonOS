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

pub type PosixTimeT = c_longlong;
pub type PosixSusecondsT = c_int;

#[repr(C)]
#[derive(Default, Debug, Copy, Clone)]
pub struct PosixTimeval {
    pub tv_sec: PosixTimeT,
    pub tv_usec: PosixSusecondsT,
}

#[repr(C)]
#[derive(Default, Debug, Copy, Clone)]
pub struct Itimerval {
    pub it_interval: PosixTimeval,
    pub it_value: PosixTimeval,
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
