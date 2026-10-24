use core::fmt::{self, Write};

use log::{info, Level, Log};

use super::lib_ui::textui::{textui_putstr, FontColor};

use crate::{
    debug::klog::loglevel::{LogLevel, KERNEL_LOG_LEVEL},
    driver::tty::{tty_driver::TtyOperation, virtual_terminal::vc_manager},
    filesystem::procfs::{
        klog::LogMessage,
        kmsg::{notify_readers, KMSG},
    },
    time::PosixTimeSpec,
};

#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => ($crate::libs::printk::__printk(format_args!($($arg)*)));
}

#[macro_export]
macro_rules! println {
    () => {
        $crate::print!("\n");
    };
    ($($arg:tt)*) => ($crate::print!("{}\n", format_args!($($arg)*)));
}

pub struct PrintkWriter;

impl PrintkWriter {
    #[inline]
    pub fn __write_fmt(&mut self, args: fmt::Arguments) {
        self.write_fmt(args).ok();
    }

    /// 并输出白底黑字
    /// @param str: 要写入的字符
    pub fn __write_string(&mut self, s: &str) {
        if let Some(current_vc) = vc_manager().current_vc() {
            // tty已经初始化了之后才输出到屏幕
            let port = current_vc.port();
            let tty = port.port_data().internal_tty();
            if let Some(tty) = tty {
                let _ = tty.write(tty.core(), s.as_bytes(), s.len());
            } else {
                let _ = textui_putstr(s, FontColor::WHITE, FontColor::BLACK);
            }
        } else {
            let _ = textui_putstr(s, FontColor::WHITE, FontColor::BLACK);
        }
    }
}

/// 为Printk Writer实现core::fmt::Write, 使得能够借助Rust自带的格式化组件，格式化字符并输出
impl fmt::Write for PrintkWriter {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.__write_string(s);
        Ok(())
    }
}

#[doc(hidden)]
pub fn __printk(args: fmt::Arguments) {
    PrintkWriter.write_fmt(args).unwrap();
}

const BOUNDED_LOG_CAPACITY: usize = 384;

/// Stack-only diagnostic formatting for allocation-constrained paths.
/// Arguments must not allocate from their own Display/Debug implementations.
pub(crate) struct BoundedLogRecord {
    bytes: [u8; BOUNDED_LOG_CAPACITY],
    len: usize,
}

impl BoundedLogRecord {
    pub(crate) fn new() -> Self {
        Self {
            bytes: [0; BOUNDED_LOG_CAPACITY],
            len: 0,
        }
    }

    pub(crate) fn as_str(&self) -> &str {
        core::str::from_utf8(self.as_bytes()).expect("UTF-8 diagnostic record")
    }

    pub(crate) fn as_bytes(&self) -> &[u8] {
        &self.bytes[..self.len]
    }
}

impl Write for BoundedLogRecord {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        let mut count = text.len().min(BOUNDED_LOG_CAPACITY - self.len);
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

pub struct Logger;

impl Logger {
    /// Format without growing a buffer, then use the fallible existing sink.
    pub(crate) fn try_log_fmt(&self, level: LogLevel, arguments: fmt::Arguments<'_>) {
        let mut record = BoundedLogRecord::new();
        let _ = record.write_fmt(arguments);
        self.try_log_text(level, record.as_str());
    }

    /// Best-effort insertion into the existing fixed ring. All allocation is
    /// fallible and outside the log lock; a busy logger cannot block recovery.
    pub(crate) fn try_log_text(&self, level: LogLevel, text: &str) {
        let Some(kmsg) = (unsafe { KMSG.as_ref() }) else {
            return;
        };
        let Ok(message) = LogMessage::try_new(
            PosixTimeSpec::now_cpu_time(),
            level,
            format_args!("{}", text),
        ) else {
            return;
        };
        let Ok(record) = alloc::sync::Arc::try_new(message) else {
            return;
        };
        // Kmsg::push 返回被驱逐消息，必须在锁外 drop：锁内释放堆内存可能
        // 进入分配器慢路径而睡眠（持自旋锁睡眠 = 永久楔死）。
        let mut pending = Some(record);
        let evicted = match kmsg.try_lock_irqsave() {
            Ok(mut kmsg) => kmsg.push(pending.take().unwrap()),
            Err(_) => None,
        };
        let published = pending.is_none();
        drop(evicted);
        drop(pending);
        if published {
            notify_readers();
        }
    }

    pub fn log(&self, log_level: LogLevel, message: fmt::Arguments) {
        let Some(kmsg) = (unsafe { KMSG.as_ref() }) else {
            return;
        };
        let Ok(record) = LogMessage::try_new(PosixTimeSpec::now_cpu_time(), log_level, message)
            .and_then(|record| {
                alloc::sync::Arc::try_new(record).map_err(|_| system_error::SystemError::ENOMEM)
            })
        else {
            return;
        };
        let evicted = kmsg.lock_irqsave().push(record);
        drop(evicted);
        notify_readers();
    }
}

/// 内核自定义日志器
///
/// todo: https://github.com/DragonOS-Community/DragonOS/issues/762
struct KernelLogger;

impl Log for KernelLogger {
    fn enabled(&self, _metadata: &log::Metadata) -> bool {
        // Console policy must not discard records from the kernel log ring.
        true
    }

    fn log(&self, record: &log::Record) {
        if self.enabled(record.metadata()) {
            // 记录到 kmsg 缓冲区
            Self::kernel_log(record);
            // The console threshold only controls the real console sink.
            if KERNEL_LOG_LEVEL.should_print(record.level().into()) {
                Self::iodisplay(record)
            }
        }
    }

    fn flush(&self) {
        // 如果需要的话，可以在这里实现缓冲区刷新逻辑
    }
}

impl KernelLogger {
    fn iodisplay(record: &log::Record) {
        match record.level() {
            Level::Debug | Level::Info | Level::Trace => {
                write!(PrintkWriter, "[ {} ] ", record.level(),)
            }
            Level::Error => {
                write!(PrintkWriter, "\x1B[41m[ ERROR ] \x1B[0m",)
            }
            Level::Warn => {
                write!(PrintkWriter, "\x1B[1;33m[ WARN ] \x1B[0m",)
            }
        }
        .unwrap();
        writeln!(
            PrintkWriter,
            "({}:{})\t {}",
            record.file().unwrap_or(""),
            record.line().unwrap_or(0),
            record.args()
        )
        .unwrap();
    }

    fn kernel_log(record: &log::Record) {
        match record.level() {
            Level::Debug | Level::Trace => Logger.log(
                LogLevel::DEBUG,
                format_args!(
                    "({}:{})\t {}\n",
                    record.file().unwrap_or(""),
                    record.line().unwrap_or(0),
                    record.args()
                ),
            ),
            Level::Error => Logger.log(
                LogLevel::ERR,
                format_args!(
                    "({}:{})\t {}\n",
                    record.file().unwrap_or(""),
                    record.line().unwrap_or(0),
                    record.args()
                ),
            ),
            Level::Info => Logger.log(
                LogLevel::INFO,
                format_args!(
                    "({}:{})\t {}\n",
                    record.file().unwrap_or(""),
                    record.line().unwrap_or(0),
                    record.args()
                ),
            ),
            Level::Warn => Logger.log(
                LogLevel::WARN,
                format_args!(
                    "({}:{})\t {}\n",
                    record.file().unwrap_or(""),
                    record.line().unwrap_or(0),
                    record.args()
                ),
            ),
        }
    }
}

pub fn early_init_logging() {
    log::set_logger(&KernelLogger).unwrap();
    log::set_max_level(log::LevelFilter::Debug);
    info!("Logging initialized");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncates_on_utf8_boundary_without_allocation() {
        let mut record = BoundedLogRecord::new();
        for _ in 0..127 {
            record.write_str("中").unwrap();
        }
        assert!(record.write_str("中文").is_err());
        assert_eq!(record.as_bytes().len(), BOUNDED_LOG_CAPACITY);
        assert!(core::str::from_utf8(record.as_bytes()).is_ok());
        assert_eq!(record.as_str().as_bytes(), record.as_bytes());
        assert!(record.write_str("x").is_err());
    }
}
