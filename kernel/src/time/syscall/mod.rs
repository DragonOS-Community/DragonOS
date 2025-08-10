use core::{
    ffi::{c_int, c_longlong},
    time::Duration,
};
use num_traits::FromPrimitive;
use system_error::SystemError;

use crate::{
    process::{ProcessManager, timer::AlarmTimer},
    syscall::{Syscall, user_access::UserBufferWriter},
    time::PosixTimeSpec,
};

use super::timekeeping::{do_gettimeofday, getnstimeofday};
mod sys_nanosleep;

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

    pub fn gettimeofday(
        tv: *mut PosixTimeval,
        timezone: *mut PosixTimeZone,
    ) -> Result<usize, SystemError> {
        // TODO; 处理时区信息
        if tv.is_null() {
            return Err(SystemError::EFAULT);
        }
        let mut tv_buf =
            UserBufferWriter::new::<PosixTimeval>(tv, core::mem::size_of::<PosixTimeval>(), true)?;

        let tz_buf = if timezone.is_null() {
            None
        } else {
            Some(UserBufferWriter::new::<PosixTimeZone>(
                timezone,
                core::mem::size_of::<PosixTimeZone>(),
                true,
            )?)
        };

        let posix_time = do_gettimeofday();

        tv_buf.copy_one_to_user(&posix_time, 0)?;

        if let Some(mut tz_buf) = tz_buf {
            tz_buf.copy_one_to_user(&SYS_TIMEZONE, 0)?;
        }

        return Ok(0);
    }

    pub fn clock_gettime(clock_id: c_int, tp: *mut PosixTimeSpec) -> Result<usize, SystemError> {
        let clock_id = PosixClockID::try_from(clock_id)?;
        if clock_id != PosixClockID::Realtime {
            // warn!("clock_gettime: currently only support Realtime clock, but got {:?}. Defaultly return realtime!!!\n", clock_id);
        }
        if tp.is_null() {
            return Err(SystemError::EFAULT);
        }
        let mut tp_buf = UserBufferWriter::new::<PosixTimeSpec>(
            tp,
            core::mem::size_of::<PosixTimeSpec>(),
            true,
        )?;

        let timespec = getnstimeofday();

        tp_buf.copy_one_to_user(&timespec, 0)?;

        return Ok(0);
    }
    /// # alarm函数功能
    ///  
    /// 设置alarm（单位：秒）
    ///
    /// ## 函数参数
    ///
    /// expired_second：设置alarm触发的秒数
    ///
    /// ### 函数返回值
    ///
    /// Ok(usize): 上一个alarm的剩余秒数
    pub fn alarm(expired_second: u32) -> Result<usize, SystemError> {
        //初始化second
        let second = Duration::from_secs(expired_second as u64);
        let pcb = ProcessManager::current_pcb();
        let mut pcb_alarm = pcb.alarm_timer_irqsave();
        let alarm = pcb_alarm.as_ref();
        //alarm第一次调用
        if alarm.is_none() {
            //注册alarm定时器
            let pid = ProcessManager::current_pid();
            let new_alarm = Some(AlarmTimer::alarm_timer_init(pid, 0));
            *pcb_alarm = new_alarm;
            drop(pcb_alarm);
            return Ok(0);
        }
        //查询上一个alarm的剩余时间和重新注册alarm
        let alarmtimer = alarm.unwrap();
        let remain = alarmtimer.remain();
        if second.is_zero() {
            alarmtimer.cancel();
        }
        if !alarmtimer.timeout() {
            alarmtimer.cancel();
        }
        let pid = ProcessManager::current_pid();
        let new_alarm = Some(AlarmTimer::alarm_timer_init(pid, second.as_secs()));
        *pcb_alarm = new_alarm;
        drop(pcb_alarm);
        return Ok(remain.as_secs() as usize);
    }
}
