//! Bounded best-effort OOM diagnostics: no infallible heap or ordinary TTY path.

use crate::{debug::klog::loglevel::LogLevel, libs::printk::Logger};
use core::fmt::{self, Write};

const CAPACITY: usize = 384;

struct Record {
    bytes: [u8; CAPACITY],
    len: usize,
}

impl Write for Record {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        let mut count = text.len().min(CAPACITY - self.len);
        while !text.is_char_boundary(count) {
            count -= 1;
        }
        self.bytes[self.len..self.len + count].copy_from_slice(&text.as_bytes()[..count]);
        self.len += count;
        if count == text.len() {
            Ok(())
        } else {
            Err(fmt::Error)
        }
    }
}

/// Arguments must themselves be allocation-free (numeric fields, static text).
/// A busy sink or exhausted heap may lose this diagnostic, never OOM progress.
pub(crate) fn report(arguments: fmt::Arguments<'_>) {
    let mut record = Record {
        bytes: [0; CAPACITY],
        len: 0,
    };
    let _ = record.write_fmt(arguments);
    let text = core::str::from_utf8(&record.bytes[..record.len]).expect("UTF-8 record");
    Logger.try_log_text(LogLevel::ERR, text);

    #[cfg(target_arch = "x86_64")]
    {
        use crate::time::{Duration, Instant};
        let deadline = Instant::now() + Duration::from_millis(10);
        let mut written = 0;
        for _ in 0..24 {
            if written == record.len || Instant::now() >= deadline {
                break;
            }
            let count = crate::driver::serial::serial8250::emergency_output(
                &record.bytes[written..record.len],
                deadline,
            );
            if count == 0 {
                break;
            }
            written += count;
        }
    }
    #[cfg(target_arch = "riscv64")]
    crate::arch::driver::sbi::console_putstr(&record.bytes[..record.len]);
    // LoongArch's existing UART polls without a deadline. Preserve the fallible
    // kmsg record there instead of making OOM recovery wait indefinitely.
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn truncates_on_utf8_boundary_without_allocation() {
        let mut record = Record {
            bytes: [0; CAPACITY],
            len: 0,
        };
        for _ in 0..127 {
            record.write_str("中").unwrap();
        }
        assert!(record.write_str("中文").is_err());
        assert_eq!(record.len, CAPACITY);
        assert!(core::str::from_utf8(&record.bytes[..record.len]).is_ok());
        assert!(record.write_str("x").is_err());
    }
}
