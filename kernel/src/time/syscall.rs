use core::ffi::{c_int, c_longlong};

use num_traits::FromPrimitive;
use system_error::SystemError;
use x86::current;

use crate::{
    process::{timer::alarm_timer_init, ProcessControlBlock, ProcessManager}, syscall::{user_access::UserBufferWriter, Syscall}, time::{sleep::nanosleep, TimeSpec}
};

use super::{timekeeping::{do_gettimeofday, getnstimeofday}, timer::{next_n_ms_timer_jiffies, timer_jiffies_n_s}};

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
    /// @brief 休眠指定时间（单位：纳秒）（提供给C的接口）
    ///
    /// @param sleep_time 指定休眠的时间
    ///
    /// @param rm_time 剩余休眠时间（传出参数）
    ///
    /// @return Ok(i32) 0
    ///
    /// @return Err(SystemError) 错误码
    pub fn nanosleep(
        sleep_time: *const TimeSpec,
        rm_time: *mut TimeSpec,
    ) -> Result<usize, SystemError> {
        if sleep_time.is_null() {
            return Err(SystemError::EFAULT);
        }
        let slt_spec = TimeSpec {
            tv_sec: unsafe { *sleep_time }.tv_sec,
            tv_nsec: unsafe { *sleep_time }.tv_nsec,
        };

        let r: Result<usize, SystemError> = nanosleep(slt_spec).map(|slt_spec| {
            if !rm_time.is_null() {
                unsafe { *rm_time }.tv_sec = slt_spec.tv_sec;
                unsafe { *rm_time }.tv_nsec = slt_spec.tv_nsec;
            }
            0
        });

        return r;
    }

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

    pub fn clock_gettime(clock_id: c_int, tp: *mut TimeSpec) -> Result<usize, SystemError> {
        let clock_id = PosixClockID::try_from(clock_id)?;
        if clock_id != PosixClockID::Realtime {
            kwarn!("clock_gettime: currently only support Realtime clock, but got {:?}. Defaultly return realtime!!!\n", clock_id);
        }
        if tp.is_null() {
            return Err(SystemError::EFAULT);
        }
        let mut tp_buf =
            UserBufferWriter::new::<TimeSpec>(tp, core::mem::size_of::<TimeSpec>(), true)?;

        let timespec = getnstimeofday();

        tp_buf.copy_one_to_user(&timespec, 0)?;

        return Ok(0);
    }

    pub fn alarm(second: u64) -> Result<usize, SystemError>{
        println!("call alarm second: {}", second);
        //第一次调用alarm需要初始化
        let current_pid = ProcessManager::current_pid();
        let binding = ProcessManager::ref_alarm_timer();
        let mut current_alarm_guard = binding.lock();
        let alarm_timer_option = current_alarm_guard.as_mut();
        match alarm_timer_option {
            Some(alarm_timer) => {
                println!("have old alarm");
                let remain_time = alarm_timer.remain();
                let remain_second = timer_jiffies_n_s(remain_time);
                if second == 0 {
                    println!("second is 0, cancel the alarm");
                    //clear timer
                    //alarm_timer.inner().expire_jiffies = 0;
                    //这里的cancel是不是只是把定时器弹出定时器列表
                    alarm_timer.cancel();
                    return Ok(remain_second as usize);
                }
                //这里的second是以jiddies为单位
                //Todo：秒转换成jiddies
                let new_expired_time = next_n_ms_timer_jiffies(second * 1_000);
                if remain_second == 0 {
                    println!("because old alarm ring, begin reset!");
                    alarm_timer.reset(new_expired_time);
                    return Ok(remain_second as usize);
                }else {
                    println!("old alarm not rings.");
                    alarm_timer.cancel();
                    alarm_timer.reset(new_expired_time);
                    return Ok(remain_second as usize);
                }    
            }
            None => {
                drop(current_alarm_guard);
                let new_expired_time = next_n_ms_timer_jiffies(second * 1_000);
                alarm_timer_init(current_pid, new_expired_time);
                return Ok(0);
            }
        }
    }
}
