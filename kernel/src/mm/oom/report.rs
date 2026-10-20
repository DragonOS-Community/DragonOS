//! Bounded best-effort OOM diagnostics: no infallible heap or ordinary TTY path.

use crate::{
    debug::klog::loglevel::LogLevel,
    libs::printk::{BoundedLogRecord, Logger},
};
use core::fmt::{self, Write};

/// Arguments must themselves be allocation-free (numeric fields, static text).
/// A busy sink or exhausted heap may lose this diagnostic, never OOM progress.
pub(crate) fn report(arguments: fmt::Arguments<'_>) {
    let mut record = BoundedLogRecord::new();
    let _ = record.write_fmt(arguments);
    Logger.try_log_text(LogLevel::ERR, record.as_str());

    #[cfg(target_arch = "x86_64")]
    {
        use crate::time::{Duration, Instant};
        let deadline = Instant::now() + Duration::from_millis(10);
        let bytes = record.as_bytes();
        let mut written = 0;
        for _ in 0..24 {
            if written == bytes.len() || Instant::now() >= deadline {
                break;
            }
            let count =
                crate::driver::serial::serial8250::emergency_output(&bytes[written..], deadline);
            if count == 0 {
                break;
            }
            written += count;
        }
    }
    #[cfg(target_arch = "riscv64")]
    crate::arch::driver::sbi::console_putstr(record.as_bytes());
    // LoongArch's existing UART polls without a deadline. Preserve the fallible
    // kmsg record there instead of making OOM recovery wait indefinitely.
}
