use core::fmt::Debug;

use alloc::{string::String, sync::Arc};
use system_error::SystemError;

use crate::{
    libs::rwlock::RwLock,
    time::{Instant, PosixTimeSpec, NSEC_PER_SEC},
};

use self::sysfs::RtcGeneralDevice;

use super::base::device::Device;

pub mod class;
pub mod interface;
pub mod rtc_cmos;
mod sysfs;
mod utils;

/// 全局默认rtc
static GLOBAL_DEFAULT_RTC: RwLock<Option<Arc<RtcGeneralDevice>>> = RwLock::new(None);

/// 获取全局的默认的rtc
fn global_default_rtc() -> Option<Arc<RtcGeneralDevice>> {
    GLOBAL_DEFAULT_RTC.read().clone()
}

/// 注册默认的rtc
fn register_default_rtc(general_device: Arc<RtcGeneralDevice>) -> bool {
    let upg = GLOBAL_DEFAULT_RTC.upgradeable_read();
    if let Some(old_dev) = upg.as_ref() {
        if old_dev.priority() >= general_device.priority() {
            return false;
        }
    }

    let mut write = upg.upgrade();

    write.replace(general_device);

    return true;
}

/// RTC设备的trait
pub trait RtcDevice: Device {
    fn class_ops(&self) -> &'static dyn RtcClassOps;
}

#[allow(dead_code)]
pub trait RtcClassOps: Send + Sync + Debug {
    fn read_time(&self, dev: &Arc<dyn RtcDevice>) -> Result<RtcTime, SystemError>;
    fn set_time(&self, dev: &Arc<dyn RtcDevice>, time: &RtcTime) -> Result<(), SystemError>;
}

#[derive(Default, Debug, Clone, Copy)]
pub struct RtcTime {
    /// `second`: 秒，范围从 0 到 59
    pub second: i32,
    /// `minute`: 分钟，范围从 0 到 59
    pub minute: i32,
    /// `hour`: 小时，范围从 0 到 23
    pub hour: i32,
    /// `mday`: 一个月中的第几天，范围从 1 到 31
    pub mday: i32,
    /// `month`: 月份，范围从 0 到 11
    pub month: i32,
    /// `year`: 年份(相对于1900年)
    pub year: i32,
    /// `wday`: 一周中的第几天，范围从 0（周日）到 6（周六）
    pub wday: i32,
    /// `yday`: 一年中的第几天，范围从 1 到 366
    pub yday: i32,
    /// `isdst`: 表示是否为夏令时，通常如果是夏令时，此值为正，否则为零或负
    pub isdst: i32,
}

impl RtcTime {
    /// 返回一个格式化的日期字符串
    ///
    /// 例如，如果日期为2021年1月1日，则返回"2021-01-01"
    pub fn date_string(&self) -> String {
        format!(
            "{:04}-{:02}-{:02}",
            self.year_real(),
            self.month_real(),
            self.mday
        )
    }

    /// 返回一个格式化的时间字符串
    ///
    /// 例如，如果时间为12:00:00，则返回"12:00:00"
    pub fn time_string(&self) -> String {
        format!("{:02}:{:02}:{:02}", self.hour, self.minute, self.second)
    }

    pub fn valid(&self) -> bool {
        !(self.year < 70
            || self.year > (i32::MAX - 1900)
            || self.month >= 12
            || self.month < 0
            || self.mday < 1
            || self.mday > self.month_days()
            || self.hour >= 24
            || self.hour < 0
            || self.minute >= 60
            || self.minute < 0
            || self.second >= 60
            || self.second < 0)
    }

    /// 获取当月有多少天
    pub fn month_days(&self) -> i32 {
        let days_in_month = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
        let month = self.month as usize;
        let mut days = days_in_month[month];

        if month == 1 && self.is_leap_year() {
            days += 1;
        }

        days
    }

    /// 判断当前是否为闰年
    pub fn is_leap_year(&self) -> bool {
        let year = self.year + 1900;
        (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
    }

    pub fn year_real(&self) -> i32 {
        self.year + 1900
    }

    pub fn month_real(&self) -> i32 {
        self.month + 1
    }
}

impl From<RtcTime> for PosixTimeSpec {
    fn from(val: RtcTime) -> Self {
        let instant = Instant::mktime64(
            val.year_real() as u32,
            (val.month + 1) as u32,
            (val.mday) as u32,
            val.hour as u32,
            val.minute as u32,
            val.second as u32,
        );

        /* 注意：RTC 仅存储完整的秒数。它是任意的，它存储的是最接近的值还是
         * 分割秒数截断的值。然而，重要的是我们使用它来存储截断的值。这是因为
         * 否则在 RTC 同步函数中，需要读取 both xtime.tv_sec 和 xtime.tv_nsec。
         * 在某些处理器上（例如 ARM），对 >32位的原子读取是不可行的。所以，
         * 存储最接近的值会减慢同步 API 的速度。因此，这里我们存储截断的值
         * 并在后面加上 0.5s 的最佳猜测。
         */
        PosixTimeSpec::new(instant.secs(), (NSEC_PER_SEC >> 1).into())
    }
}

/// 全局默认rtc的优先级
#[allow(dead_code)]
#[derive(Debug, Default, PartialEq, PartialOrd, Clone, Copy)]
pub enum GeneralRtcPriority {
    #[default]
    L0 = 0,
    L1,
}
