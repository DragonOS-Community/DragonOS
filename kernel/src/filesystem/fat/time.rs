//! FAT's local-calendar disk format. POSIX ctime is the modification time,
//! not the independent creation timestamp stored in the directory entry.
use crate::time::{syscall::SYS_TIMEZONE, Instant, PosixTimeSpec};

const DAY: i64 = 86400;
const EPOCH: i64 = 315532800; // 1980-01-01 00:00:00, before the timezone offset.

fn offset() -> i64 {
    i64::from(SYS_TIMEZONE.tz_minuteswest) * 60
}

fn leap(year: i64) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

fn month_days(year: i64, month: usize) -> i64 {
    const DAYS: [i64; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    DAYS[month - 1] + i64::from(month == 2 && leap(year))
}

/// notify_change()/current_time() first apply the filesystem's supported
/// range before FAT's own precision rules (Linux fs/attr.c and fs/inode.c).
fn clamp(mut time: PosixTimeSpec) -> PosixTimeSpec {
    let minimum = EPOCH + offset();
    let maximum = Instant::mktime64(2107, 12, 31, 23, 59, 58).secs() + offset();
    time.tv_sec = time.tv_sec.clamp(minimum, maximum);
    if time.tv_sec == minimum || time.tv_sec == maximum {
        time.tv_nsec = 0;
    }
    time
}

pub(super) fn truncate_creation(time: PosixTimeSpec) -> PosixTimeSpec {
    let mut time = clamp(time);
    time.tv_nsec -= time.tv_nsec % 10_000_000;
    time
}

pub(super) fn truncate_mtime(time: PosixTimeSpec) -> PosixTimeSpec {
    PosixTimeSpec::new(clamp(time).tv_sec & !1, 0)
}

pub(super) fn truncate_atime(time: PosixTimeSpec) -> PosixTimeSpec {
    // Apply the VFS range first, then fat_truncate_atime's local day boundary.
    let local = i128::from(clamp(time).tv_sec) - i128::from(offset());
    let seconds = local - local % i128::from(DAY) + i128::from(offset());
    PosixTimeSpec::new(
        seconds.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64,
        0,
    )
}

/// (time, date, creation centiseconds). Only disk encoding saturates the
/// calendar to FAT's range. The inode owner also applies the Linux VFS range
/// before precision truncation; the codec remains safe for arbitrary input.
pub(super) fn encode(value: PosixTimeSpec) -> (u16, u16, u8) {
    let local = i128::from(value.tv_sec) - i128::from(offset());
    let maximum = i128::from(Instant::mktime64(2107, 12, 31, 23, 59, 59).secs());
    if local < i128::from(EPOCH) {
        return (0, 33, 0);
    }
    if local > maximum {
        return (
            (23 << 11) | (59 << 5) | 29,
            (127 << 9) | (12 << 5) | 31,
            199,
        );
    }
    let local = local as i64;
    let mut days = (local - EPOCH) / DAY;
    let mut year = 1980;
    // The encoded calendar has only 128 years; these loops are bounded even
    // for extreme syscall inputs and correctly exclude 2100's leap day.
    while days >= if leap(year) { 366 } else { 365 } {
        days -= if leap(year) { 366 } else { 365 };
        year += 1;
    }
    let mut month = 1;
    while days >= month_days(year, month) {
        days -= month_days(year, month);
        month += 1;
    }
    let seconds = local % DAY;
    (
        (((seconds / 3600) << 11) | (((seconds / 60) % 60) << 5) | ((seconds % 60) / 2)) as u16,
        (((year - 1980) << 9) | ((month as i64) << 5) | (days + 1)) as u16,
        ((value.tv_sec & 1) * 100 + value.tv_nsec / 10_000_000) as u8,
    )
}

pub(super) fn decode(time: u16, date: u16, centiseconds: u8) -> PosixTimeSpec {
    // Like Linux, old entries with zero day/month decode to the first day
    // and month rather than making their ordinary files inaccessible.
    const BEFORE_MONTH: [i64; 16] = [
        0, 0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334, 0, 0, 0,
    ];
    let year = i64::from(date >> 9);
    let month = usize::from((date >> 5) & 15).max(1);
    let day = i64::from(date & 31).max(1) - 1;
    let leap_days =
        (year + 3) / 4 - i64::from(year > 120) + i64::from(leap(1980 + year) && month > 2);
    let seconds = EPOCH
        + (year * 365 + leap_days + BEFORE_MONTH[month] + day) * DAY
        + i64::from(time & 31) * 2
        + i64::from((time >> 5) & 63) * 60
        + i64::from(time >> 11) * 3600
        + offset()
        + i64::from(centiseconds / 100);
    PosixTimeSpec::new(seconds, i64::from(centiseconds % 100) * 10_000_000)
}
