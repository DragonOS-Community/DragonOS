use super::syscall::PosixTimeT;
/// 一小时所包含的秒数
const SECS_PER_HOUR: i64 = 60 * 60;
/// 一天所包含的秒数
const SECS_PER_DAY: i64 = SECS_PER_HOUR * 24;
/// 每年中每个月最后一天所对应天数
const MON_OF_YDAY: [[i64; 13]; 2] = [
    // 普通年
    [0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334, 365],
    // 闰年
    [0, 31, 60, 91, 121, 152, 182, 213, 244, 274, 305, 335, 366],
];
#[derive(Debug)]
#[allow(dead_code)]
pub struct CalendarTime {
    tm_sec: i32,
    tm_min: i32,
    tm_hour: i32,
    tm_mday: i32,
    tm_mon: i32,
    tm_wday: i32,
    tm_yday: i32,
    tm_year: i32,
}
impl CalendarTime {
    pub fn new() -> Self {
        CalendarTime {
            tm_year: Default::default(),
            tm_sec: Default::default(),
            tm_min: Default::default(),
            tm_hour: Default::default(),
            tm_mday: Default::default(),
            tm_mon: Default::default(),
            tm_wday: Default::default(),
            tm_yday: Default::default(),
        }
    }
}

/// # 判断是否是闰年
///
/// ## 参数
///
/// * 'year' - 年份
fn is_leap(year: u32) -> bool {
    let mut flag = false;
    if (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 {
        flag = true;
    }
    return flag;
}

/// # 计算除法
///
/// # 参数
///
/// * 'left' - 被除数
/// * 'right' - 除数
fn math_div(left: u32, right: u32) -> u32 {
    return left / right;
}

/// # 计算两年之间的闰年数目
///
/// ## 参数
///
/// * 'y1' - 起始年份
/// * 'y2' - 结束年份
fn leaps_between(y1: u32, y2: u32) -> u32 {
    // 算出y1之前的闰年数量
    let y1_leaps = math_div(y1 - 1, 4) - math_div(y1 - 1, 100) + math_div(y1 - 1, 400);
    // 算出y2之前的闰年数量
    let y2_leaps = math_div(y2 - 1, 4) - math_div(y2 - 1, 100) + math_div(y2 - 1, 400);

    y2_leaps - y1_leaps
}

/// # 将秒数转换成日期
///
/// ## 参数
///
/// * 'totalsecs' - 1970年1月1日 00:00:00 UTC到现在的秒数
/// * 'offset' - 指定的秒数对应的时间段（含）的偏移量（以秒为单位）
#[allow(dead_code)]
pub fn time_to_calendar(totalsecs: PosixTimeT, offset: i32) -> CalendarTime {
    let mut result = CalendarTime::new();
    // 计算对应的天数
    let mut days = totalsecs / SECS_PER_DAY;
    // 一天中剩余的秒数
    let mut rem = totalsecs % SECS_PER_DAY;

    // 加入偏移量
    rem += offset as i64;
    while rem < 0 {
        rem += SECS_PER_DAY;
        days -= 1;
    }
    while rem >= SECS_PER_DAY {
        rem -= SECS_PER_DAY;
        days += 1;
    }
    // 计算对应的小时数
    result.tm_hour = (rem / SECS_PER_HOUR) as i32;
    rem %= SECS_PER_HOUR;

    // 计算对应的分钟数
    result.tm_min = (rem / 60) as i32;
    rem %= 60;

    // 秒数
    result.tm_sec = rem as i32;

    // totalsec是从1970年1月1日 00:00:00 UTC到现在的秒数
    // 当时是星期四
    result.tm_wday = ((4 + days) % 7) as i32;

    let mut year = 1970;
    while days < 0 || (is_leap(year) && days >= 366) || (!is_leap(year) && days >= 365) {
        // 假设每一年都是365天，计算出大概的年份
        let guess_year = year + math_div(days.try_into().unwrap(), 365);
        // 将已经计算过的天数去掉
        days -= ((guess_year - year) * 365 + leaps_between(year, guess_year)) as i64;
        year = guess_year;
    }
    result.tm_year = (year - 1900) as i32;
    result.tm_yday = days as i32;
    let mut il = 0;
    if is_leap(year) {
        il = 1
    };
    let mut mon = 0;
    for i in MON_OF_YDAY[il] {
        if days < i {
            break;
        }
        mon += 1;
    }
    days -= MON_OF_YDAY[il][mon - 1];
    result.tm_mon = (mon - 1) as i32;
    result.tm_mday = (days + 1) as i32;

    result
}
