#![allow(dead_code)]

use core::intrinsics::unlikely;

use system_error::SystemError;

use crate::driver::rtc::interface::rtc_read_time_default;

use super::{PosixTimeSpec, NSEC_PER_SEC};

// 参考：https://code.dragonos.org.cn/xref/linux-3.4.99/include/linux/time.h#110
const KTIME_MAX: i64 = !(1u64 << 63) as i64;
const KTIME_SEC_MAX: i64 = KTIME_MAX / NSEC_PER_SEC as i64;

#[allow(non_camel_case_types)]
pub type ktime_t = i64;

// @brief 将ktime_t类型转换为纳秒类型
#[inline]
fn ktime_to_ns(kt: ktime_t) -> i64 {
    return kt;
}

/// @brief 从RTC获取当前时间，然后计算时间戳。
/// 时间戳为从UTC+0 1970-01-01 00:00到当前UTC+0时间，所经过的纳秒数。
/// 注意，由于当前未引入时区，因此本函数默认时区为UTC+8来计算
fn ktime_get_real() -> Result<ktime_t, SystemError> {
    let rtc_time = rtc_read_time_default()?;
    let time_spec: PosixTimeSpec = rtc_time.into();
    let r = time_spec.tv_sec * 1_000_000_000 + time_spec.tv_nsec;
    return Ok(r);
}

/// @brief 暴露给外部使用的接口，返回一个时间戳
#[inline]
pub fn ktime_get_real_ns() -> i64 {
    let kt: ktime_t = ktime_get_real().unwrap_or(0);
    return ktime_to_ns(kt);
}

// # 用于将两个ktime_t类型的变量相加
// #[inline(always)]
// pub(super) fn ktime_add(add1: ktime_t, add2: ktime_t) -> ktime_t {
//     let res = add1 + add2;
// }

/// # 通过sec和nsec构造一个ktime_t
#[inline(always)]
fn ktime_set(secs: i64, nsecs: u64) -> ktime_t {
    if unlikely(secs >= KTIME_SEC_MAX) {
        return KTIME_MAX;
    }

    return secs * NSEC_PER_SEC as i64 + nsecs as i64;
}

/// # 将PosixTimeSpec转换成ktime_t
#[inline(always)]
pub fn timespec_to_ktime(ts: PosixTimeSpec) -> ktime_t {
    return ktime_set(ts.tv_sec, ts.tv_nsec as u64);
}
