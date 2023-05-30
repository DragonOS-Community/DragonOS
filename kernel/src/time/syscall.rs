use core::{
    ffi::{c_int, c_longlong},
    intrinsics::size_of,
    ptr::null_mut,
};

use crate::{
    include::bindings::bindings::{pt_regs, verify_area},
    syscall::{Syscall, SystemError},
    time::{sleep::nanosleep, timeconv::time_to_calendar, TimeSpec},
};

use super::timekeeping::do_gettimeofday;

pub type PosixTimeT = c_longlong;
pub type PosixSusecondsT = c_int;

#[repr(C)]
#[derive(Default)]
pub struct PosixTimeval {
    pub tv_sec: PosixTimeT,
    pub tv_usec: PosixSusecondsT,
}

#[repr(C)]
#[derive(Default)]
/// 当前时区信息
pub struct PosixTimezone {
    /// 格林尼治相对于当前时区相差的分钟数
    pub tz_minuteswest: c_int,
    /// DST矫正时差
    pub tz_dsttime: c_int,
}

/// 系统时区 暂时写定为东八区
pub const SYS_TIMEZONE: PosixTimezone = PosixTimezone {
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

    pub fn sys_do_gettimeofday(tv: *mut PosixTimeval) -> Result<usize, SystemError> {
        if tv == null_mut() {
            return Err(SystemError::EFAULT);
        }
        let posix_time = do_gettimeofday();
        unsafe {
            (*tv).tv_sec = posix_time.tv_sec;
            (*tv).tv_usec = posix_time.tv_usec;
        }
        return Ok(0);
        // time_to_calendar(posix_time.tv_sec, 0);
    }
}

// #[no_mangle]
// pub extern "C" fn sys_gettimeofday(regs: &pt_regs) -> u64 {
//     if unsafe {
//         !verify_area(regs.r8, size_of::<PosixTimeval>() as u64)
//             || !verify_area(regs.r9, size_of::<PosixTimezone>() as u64)
//     } {
//         return SystemError::EPERM as u64;
//     }
//     let timeval = regs.r8 as *mut PosixTimeval;
//     let mut timezone = regs.r9 as *const PosixTimezone;
//     if !timeval.is_null() {
//         rs_do_gettimeofday(timeval);
//     }
//     if !timezone.is_null() {
//         timezone = &SYS_TIMEZONE;
//     }
//     return 0;
// }
