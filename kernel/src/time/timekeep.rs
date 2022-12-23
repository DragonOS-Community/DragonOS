#![allow(dead_code)]

use crate::driver::timers::rtc::rtc::{rtc_get_cmos_time, rtc_time_t};

#[allow(non_camel_case_types)]
pub type ktime_t = i64;

// @brief 将ktime_t类型转换为纳秒类型
#[inline]
fn ktime_to_ns(kt: ktime_t) -> i64 {
    return kt as i64;
}

/// @brief 从RTC获取当前时间，然后计算时间戳。
/// 时间戳为从UTC+0 1970-01-01 00:00到当前UTC+0时间，所经过的纳秒数。
/// 注意，由于当前未引入时区，因此本函数默认时区为UTC+8来计算
fn ktime_get_real() -> ktime_t {
    let mut rtc_time: rtc_time_t = rtc_time_t {
        second: (0),
        minute: (0),
        hour: (0),
        day: (0),
        month: (0),
        year: (0),
    };

    //调用rtc.h里面的函数
    rtc_get_cmos_time(&mut rtc_time);

    let mut day_count: i32 = 0;
    for year in 1970..rtc_time.year {
        let leap: bool = (year % 4 == 0) && (year % 100 != 0) || (year % 400 == 0);
        if leap {
            day_count += 366;
        } else {
            day_count += 365;
        }
    }
    for month in 1..rtc_time.month {
        match month {
            1 | 3 | 5 | 7 | 8 | 10 | 12 => day_count += 31,
            2 => day_count += 28,
            4 | 6 | 9 | 11 => day_count += 30,
            _ => day_count += 0,
        }
    }

    day_count += rtc_time.day - 1;
    //转换成纳秒
    let timestamp: ktime_t = day_count as i64 * 86_400_000_000_000i64
        + (rtc_time.hour - 8) as i64 * 3_600_000_000_000i64
        + rtc_time.minute as i64 * 60_000_000_000i64
        + rtc_time.second as i64 * 1_000_000_000u64 as ktime_t;

    return timestamp;
}

/// @brief 暴露给外部使用的接口，返回一个时间戳
#[inline]
pub fn ktime_get_real_ns() -> i64 {
    let kt: ktime_t = ktime_get_real();
    return ktime_to_ns(kt);
}
