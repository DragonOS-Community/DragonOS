#![allow(dead_code)]

use system_error::SystemError;

use crate::driver::rtc::interface::rtc_read_time_default;

use super::TimeSpec;

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
    let time_spec: TimeSpec = rtc_time.into();
    let r = time_spec.tv_sec * 1_000_000_000 + time_spec.tv_nsec;
    return Ok(r);
}

/// @brief 暴露给外部使用的接口，返回一个时间戳
#[inline]
pub fn ktime_get_real_ns() -> i64 {
    let kt: ktime_t = ktime_get_real().unwrap_or(0);
    return ktime_to_ns(kt);
}
