use core::{
    ffi::{c_int, c_longlong},
    ptr::null_mut,
};

use crate::{
    syscall::{user_access::UserBufferWriter, Syscall, SystemError},
    time::{sleep::nanosleep, TimeSpec},
};

use super::timekeeping::do_gettimeofday;

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
        if sleep_time == null_mut() {
            return Err(SystemError::EFAULT);
        }
        let slt_spec = TimeSpec {
            tv_sec: unsafe { *sleep_time }.tv_sec,
            tv_nsec: unsafe { *sleep_time }.tv_nsec,
        };

        let r: Result<usize, SystemError> = nanosleep(slt_spec).map(|slt_spec| {
            if rm_time != null_mut() {
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
}
