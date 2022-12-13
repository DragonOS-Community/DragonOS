/// author: Zhi Zu Chang Le
/// date:   2022/12/13 
#[allow(non_camel_case_types)]

use crate::{include::bindings::bindings::{rtc_get_cmos_time, rtc_time_t}, println};

pub type ktime_t = i64;

// @brief 将ktime_t类型转换为纳秒类型
fn ktime_to_ns(kt:ktime_t)->i64{
    return kt as i64;
}
// @brief 通过rtc时钟得到墙上时间到1970年1月1日8:00am的时间差
fn ktime_get_real()->ktime_t{
    let mut rtc_time:rtc_time_t=rtc_time_t { 
        second: (0), 
        minute: (0), 
        hour: (0), 
        day: (0), 
        month: (0), 
        year: (0) };

     unsafe{
    //调用rtc.h里面的函数
    rtc_get_cmos_time(&mut rtc_time);
     }
    println!("{year}-{month}-{day}-{hour}-{minute}-{second}\n",
    year=rtc_time.year,month=rtc_time.month,day=rtc_time.day,
    hour=rtc_time.hour,
    minute=rtc_time.minute,
    second=rtc_time.second);
    let mut day_count:i32=0;
    for year in 1970..rtc_time.year{
        let leap:bool=(year%4==0)&&(year%100!=0)||(year%400==0);
        if leap{
            day_count+=366;
        }
        else {
            day_count+=365;
        }
        //println!("{},{}",year,day_count);
    }
    //println!("day count1: {}",day_count);
    for month in 1..rtc_time.month{
        match month{
            1|3|5|7|8|10|12 => day_count+=31,
            2 => day_count+=28,
            4|6|9|11=>day_count+=30,
            _=>day_count+=0,
        }
        //println!("{}:{}",month,day_count);
    }
    
    day_count+=rtc_time.day-1;
    //println!("day count2: {}",day_count);
    //转换成纳秒
    let timestamp:ktime_t=
    day_count as i64            *86_400_000_000_000i64
    +(rtc_time.hour-8) as i64   * 3_600_000_000_000i64
    +rtc_time.minute as i64     *    60_000_000_000i64
    +rtc_time.second as i64     *     1_000_000_000i64 
    as ktime_t; 
    
    return timestamp;
}

/// @brief 暴露给外部使用的接口，返回一个时间戳
pub fn ktime_get_real_ns()->i64{
    let kt:ktime_t=ktime_get_real();
    return ktime_to_ns(kt);
}
