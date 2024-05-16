#[allow(dead_code)]
pub const ACPI_PM_OVERRUN: u64 = 1 << 24;

/// Number of PMTMR ticks expected during calibration run
pub const PMTMR_TICKS_PER_SEC: u64 = 3579545;

/// 用于掩码ACPI_PM_READ_ERALY返回值的前24位
pub const ACPI_PM_MASK: u64 = 0xffffff;

#[inline(always)]
#[cfg(target_arch = "x86_64")]
pub fn acpi_pm_read_early() -> u32 {
    use crate::driver::clocksource::acpi_pm::{acpi_pm_read_verified, PMTMR_IO_PORT};
    use core::sync::atomic::Ordering;
    let port = PMTMR_IO_PORT.load(Ordering::SeqCst);

    // 如果端口为零直接返回
    if port == 0 {
        return 0;
    }

    // 对读取的pmtmr值进行验证并进行掩码处理
    return acpi_pm_read_verified() & ACPI_PM_MASK as u32;
}

#[inline(always)]
#[cfg(not(target_arch = "x86_64"))]
#[allow(dead_code)]
pub fn acpi_pm_read_early() -> u32 {
    return 0;
}
