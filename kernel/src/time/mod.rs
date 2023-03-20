pub mod timekeep;

/// 表示时间的结构体
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TimeSpec {
    pub tv_sec: i64,
    pub tv_nsec: i64,
}

impl TimeSpec {
    pub fn new(sec: i64, nsec: i64) -> TimeSpec {
        return TimeSpec {
            tv_sec: sec,
            tv_nsec: nsec,
        };
    }
}