//! Bounded, immutable text records shared by log snapshots and stream readers.
use alloc::string::String;
use core::fmt::{self, Write};
use system_error::SystemError;

use crate::{
    debug::klog::loglevel::{LogLevel, KERNEL_LOG_LEVEL},
    time::PosixTimeSpec,
};

/// The limit includes every line's prefix and terminating newline. Together
/// with the ring's descriptor limit this is a real bound on retained text.
pub(super) const LOG_RECORD_CAPACITY: usize = 1024;

#[derive(Debug)]
pub struct LogMessage {
    text: String,
}

impl LogMessage {
    pub fn try_new(
        timestamp: PosixTimeSpec,
        level: LogLevel,
        message: fmt::Arguments<'_>,
    ) -> Result<Self, SystemError> {
        let mut text = String::new();
        text.try_reserve_exact(LOG_RECORD_CAPACITY)
            .map_err(|_| SystemError::ENOMEM)?;
        let priority = match level {
            LogLevel::DEFAULT => KERNEL_LOG_LEVEL.get_default_message_level().min(7),
            level => level as u8,
        };
        // Only the small numeric prefix lives on the stack, never the payload.
        let mut prefix = Prefix {
            bytes: [0; 64],
            len: 0,
        };
        let _ = write!(
            prefix,
            "<{}>[{}.{:06}] ",
            priority,
            timestamp.tv_sec,
            timestamp.tv_nsec / 1000
        );
        let mut writer = RecordWriter {
            text: &mut text,
            prefix: prefix.as_str(),
            line_start: true,
        };
        let _ = writer.write_fmt(message);
        // printk strips an incoming final newline, then emits exactly one.
        // Empty messages still have a priority and a terminating newline.
        if writer.text.is_empty() {
            writer.text.push_str(writer.prefix);
            writer.line_start = false;
        }
        if !writer.line_start {
            writer.text.push('\n');
        }
        debug_assert!(text.len() <= LOG_RECORD_CAPACITY);
        Ok(Self { text })
    }

    pub fn as_bytes(&self) -> &[u8] {
        self.text.as_bytes()
    }
}

impl fmt::Display for LogMessage {
    fn fmt(&self, output: &mut fmt::Formatter<'_>) -> fmt::Result {
        output.write_str(&self.text)
    }
}

struct Prefix {
    bytes: [u8; 64],
    len: usize,
}
impl Prefix {
    fn as_str(&self) -> &str {
        core::str::from_utf8(&self.bytes[..self.len]).expect("numeric log prefix")
    }
}
impl Write for Prefix {
    fn write_str(&mut self, value: &str) -> fmt::Result {
        let end = self.len.checked_add(value.len()).ok_or(fmt::Error)?;
        let target = self.bytes.get_mut(self.len..end).ok_or(fmt::Error)?;
        target.copy_from_slice(value.as_bytes());
        self.len = end;
        Ok(())
    }
}

struct RecordWriter<'a> {
    text: &'a mut String,
    prefix: &'a str,
    line_start: bool,
}
impl Write for RecordWriter<'_> {
    fn write_str(&mut self, mut value: &str) -> fmt::Result {
        if value.is_empty() {
            return Ok(());
        }
        loop {
            if self.line_start {
                if self.text.len() + self.prefix.len() + 1 > LOG_RECORD_CAPACITY {
                    return Err(fmt::Error);
                }
                self.text.push_str(self.prefix);
                self.line_start = false;
            }
            let end = value.find('\n').unwrap_or(value.len());
            let available = LOG_RECORD_CAPACITY - self.text.len() - 1;
            let mut count = end.min(available);
            while !value.is_char_boundary(count) {
                count -= 1;
            }
            self.text.push_str(&value[..count]);
            if count != end {
                return Err(fmt::Error);
            }
            if end == value.len() {
                return Ok(());
            }
            self.text.push('\n');
            self.line_start = true;
            value = &value[end + 1..];
            if value.is_empty() {
                return Ok(());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(message: fmt::Arguments<'_>) -> LogMessage {
        LogMessage::try_new(
            PosixTimeSpec {
                tv_sec: 1,
                tv_nsec: 123456789,
            },
            LogLevel::WARN,
            message,
        )
        .unwrap()
    }

    #[test]
    fn empty_message_is_a_nonempty_complete_record() {
        assert_eq!(record(format_args!("")).as_bytes(), b"<4>[1.123456] \n");
    }

    #[test]
    fn trailing_newline_and_empty_format_piece_do_not_add_a_line() {
        assert_eq!(
            record(format_args!("{}{}", "hello\n", "")).as_bytes(),
            b"<4>[1.123456] hello\n"
        );
    }

    #[test]
    fn every_message_line_has_a_numeric_prefix() {
        assert_eq!(
            record(format_args!("a\nb\n\n")).as_bytes(),
            b"<4>[1.123456] a\n<4>[1.123456] b\n<4>[1.123456] \n"
        );
    }

    #[test]
    fn oversized_utf8_and_multiline_records_remain_bounded_and_terminated() {
        for count in 0..600 {
            let message = "界".repeat(count);
            let record = record(format_args!("{}", message));
            assert!(record.as_bytes().len() <= LOG_RECORD_CAPACITY);
            assert!(core::str::from_utf8(record.as_bytes()).is_ok());
            assert_eq!(record.as_bytes().last(), Some(&b'\n'));
        }
        let message = "a\n".repeat(1000);
        let record = record(format_args!("{}", message));
        assert!(record.as_bytes().len() <= LOG_RECORD_CAPACITY);
        assert_eq!(record.as_bytes().last(), Some(&b'\n'));
        for line in core::str::from_utf8(record.as_bytes()).unwrap().lines() {
            assert!(line.starts_with("<4>[1.123456] "));
        }
    }
}
