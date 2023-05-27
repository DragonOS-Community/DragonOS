use core::{
    ffi::{c_int, c_longlong},
    intrinsics::size_of,
};

use crate::{
    include::bindings::bindings::{pt_regs, verify_area},
    kdebug,
    syscall::SystemError, time::timeconv::time_to_calendar,
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
pub fn rs_do_gettimeofday(tv: *mut PosixTimeval) {
    let posix_time = do_gettimeofday();
    unsafe {
        (*tv).tv_sec = posix_time.tv_sec;
        (*tv).tv_usec = posix_time.tv_usec;
    }
    time_to_calendar(posix_time.tv_sec, 0);
}

#[no_mangle]
pub extern "C" fn sys_gettimeofday(regs: &pt_regs) -> u64 {
    if unsafe {
        !verify_area(regs.r8, size_of::<PosixTimeval>() as u64)
            || !verify_area(regs.r9, size_of::<PosixTimezone>() as u64)
    } {
        return SystemError::EPERM as u64;
    }
    let timeval = regs.r8 as *mut PosixTimeval;
    let mut timezone = regs.r9 as *const PosixTimezone;
    if !timeval.is_null() {
        rs_do_gettimeofday(timeval);
    }
    if !timezone.is_null() {
        timezone = &SYS_TIMEZONE;
    }
    return 0;
}
