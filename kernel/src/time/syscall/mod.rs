use core::ffi::{c_int, c_longlong};
use system_error::SystemError;

use crate::syscall::Syscall;

mod posix_clock;
#[cfg(target_arch = "x86_64")]
mod sys_alarm;
mod sys_clock_getres;
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

pub(crate) use posix_clock::{posix_clock_now, posix_clock_res};

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

/// CPU clock ID bit masks (matching Linux's include/linux/posix-timers.h)
const CPUCLOCK_PERTHREAD_MASK: u32 = 4;
const CPUCLOCK_CLOCK_MASK: u32 = 3;
const CPUCLOCK_SCHED: u32 = 2;

/// Standard POSIX clock ID constants
pub const CLOCK_REALTIME: i32 = 0;
pub const CLOCK_MONOTONIC: i32 = 1;
pub const CLOCK_PROCESS_CPUTIME_ID: i32 = 2;
pub const CLOCK_THREAD_CPUTIME_ID: i32 = 3;
pub const CLOCK_MONOTONIC_RAW: i32 = 4;
pub const CLOCK_REALTIME_COARSE: i32 = 5;
pub const CLOCK_MONOTONIC_COARSE: i32 = 6;
pub const CLOCK_BOOTTIME: i32 = 7;
pub const CLOCK_REALTIME_ALARM: i32 = 8;
pub const CLOCK_BOOTTIME_ALARM: i32 = 9;

/// The IDs of the various system clocks (for POSIX.1b interval timers).
/// The raw value is stored to support both static clock IDs and dynamic CPU clock IDs
/// (used by pthread_getcpuclockid).
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum PosixClockID {
    /// CLOCK_REALTIME
    Realtime,
    /// CLOCK_MONOTONIC
    Monotonic,
    /// CLOCK_PROCESS_CPUTIME_ID
    ProcessCPUTimeID,
    /// CLOCK_THREAD_CPUTIME_ID
    ThreadCPUTimeID,
    /// CLOCK_MONOTONIC_RAW
    MonotonicRaw,
    /// CLOCK_REALTIME_COARSE
    RealtimeCoarse,
    /// CLOCK_MONOTONIC_COARSE
    MonotonicCoarse,
    /// CLOCK_BOOTTIME
    Boottime,
    /// CLOCK_REALTIME_ALARM
    RealtimeAlarm,
    /// CLOCK_BOOTTIME_ALARM
    BoottimeAlarm,
    /// Dynamic CPU clock ID for pthread_getcpuclockid.
    /// Format: (~pid << 3) | (per_thread << 2) | clock_type
    /// This represents a CPU clock for a specific thread or process.
    DynamicCpuClock(u32),
}

impl PosixClockID {
    /// Returns true if this is a per-thread CPU clock (vs per-process)
    pub fn is_per_thread_cpu_clock(&self) -> bool {
        match self {
            PosixClockID::ThreadCPUTimeID => true,
            PosixClockID::DynamicCpuClock(raw) => {
                // For DynamicCpuClock, check bit 2 (CPUCLOCK_PERTHREAD_MASK)
                (raw & CPUCLOCK_PERTHREAD_MASK) != 0
            }
            _ => false,
        }
    }

    /// Returns the raw clock ID value
    pub fn raw_value(&self) -> i32 {
        match self {
            PosixClockID::Realtime => CLOCK_REALTIME,
            PosixClockID::Monotonic => CLOCK_MONOTONIC,
            PosixClockID::ProcessCPUTimeID => CLOCK_PROCESS_CPUTIME_ID,
            PosixClockID::ThreadCPUTimeID => CLOCK_THREAD_CPUTIME_ID,
            PosixClockID::MonotonicRaw => CLOCK_MONOTONIC_RAW,
            PosixClockID::RealtimeCoarse => CLOCK_REALTIME_COARSE,
            PosixClockID::MonotonicCoarse => CLOCK_MONOTONIC_COARSE,
            PosixClockID::Boottime => CLOCK_BOOTTIME,
            PosixClockID::RealtimeAlarm => CLOCK_REALTIME_ALARM,
            PosixClockID::BoottimeAlarm => CLOCK_BOOTTIME_ALARM,
            PosixClockID::DynamicCpuClock(raw) => *raw as i32,
        }
    }
}

impl TryFrom<i32> for PosixClockID {
    type Error = SystemError;

    fn try_from(value: i32) -> Result<Self, Self::Error> {
        // Match standard POSIX clock IDs (0-9)
        match value {
            0 => Ok(PosixClockID::Realtime),
            1 => Ok(PosixClockID::Monotonic),
            2 => Ok(PosixClockID::ProcessCPUTimeID),
            3 => Ok(PosixClockID::ThreadCPUTimeID),
            4 => Ok(PosixClockID::MonotonicRaw),
            5 => Ok(PosixClockID::RealtimeCoarse),
            6 => Ok(PosixClockID::MonotonicCoarse),
            7 => Ok(PosixClockID::Boottime),
            8 => Ok(PosixClockID::RealtimeAlarm),
            9 => Ok(PosixClockID::BoottimeAlarm),
            // Check for dynamic CPU clock IDs (negative values, i.e., bit 31 set)
            // Linux uses this format for pthread_getcpuclockid() and process-specific CPU clocks
            // Format: (~pid << 3) | (per_thread << 2) | clock_type
            // where clock_type is CPUCLOCK_PROF (0), CPUCLOCK_VIRT (1), or CPUCLOCK_SCHED (2)
            _ => {
                let raw = value as u32;

                // A valid CPU clock ID has the high bit set (negative when interpreted as i32)
                // and bits 1:0 should be 0, 1, or 2 (PROF, VIRT, or SCHED)
                if raw & 0x80000000 != 0 {
                    let clock_type = raw & CPUCLOCK_CLOCK_MASK;
                    if clock_type <= CPUCLOCK_SCHED {
                        return Ok(PosixClockID::DynamicCpuClock(raw));
                    }
                }

                Err(SystemError::EINVAL)
            }
        }
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
