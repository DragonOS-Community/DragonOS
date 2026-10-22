use core::fmt::{self, Write};

use alloc::string::{String, ToString};
use log::{info, Level, Log};

use super::lib_ui::textui::{textui_putstr, FontColor};

use crate::{
    debug::klog::loglevel::{LogLevel, KERNEL_LOG_LEVEL},
    driver::tty::{tty_driver::TtyOperation, virtual_terminal::vc_manager},
    filesystem::procfs::{klog::LogMessage, kmsg::KMSG},
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

/// 关键路径打印：绕过 tty 背压，直接同步送入控制台设备。
#[macro_export]
macro_rules! print_critical {
    ($($arg:tt)*) => ($crate::libs::printk::__printk_critical(format_args!($($arg)*)));
}

#[macro_export]
macro_rules! println_critical {
    () => ($crate::print_critical!("\n"));
    ($($arg:tt)*) => ($crate::print_critical!("{}\n", format_args!($($arg)*)));
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

/// 关键路径写入器：优先使用不受 tty 背压影响的同步控制台通道；
/// 通道不存在（如 virtio-console 尚未探测、或使用其他控制台）时退回
/// 普通 printk 路径。用于 error/warn/panic 输出，保证消息体不被丢弃。
pub struct CriticalPrintkWriter;

impl CriticalPrintkWriter {
    pub fn write_critical_string(&mut self, s: &str) {
        if crate::driver::char::virtio_console::critical_output_available() {
            crate::driver::char::virtio_console::virtio_console_critical_output(s.as_bytes());
        } else {
            PrintkWriter.__write_string(s);
        }
    }
}

impl fmt::Write for CriticalPrintkWriter {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.write_critical_string(s);
        Ok(())
    }
}

#[doc(hidden)]
pub fn __printk_critical(args: fmt::Arguments) {
    CriticalPrintkWriter.write_fmt(args).ok();
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
        let mut message = String::new();
        if message.try_reserve_exact(text.len()).is_err() {
            return;
        }
        message.push_str(text);
        let record = LogMessage::new(PosixTimeSpec::now_cpu_time(), level, message);
        // Kmsg::push 返回被驱逐消息，必须在锁外 drop：锁内释放堆内存可能
        // 进入分配器慢路径而睡眠（持自旋锁睡眠 = 永久楔死）。
        let evicted = match kmsg.try_lock_irqsave() {
            Ok(mut kmsg) => kmsg.push(record),
            Err(_) => None,
        };
        drop(evicted);
    }

    pub fn log(&self, log_level: LogLevel, message: fmt::Arguments) {
        if unsafe { KMSG.is_some() } {
            let timestamp: PosixTimeSpec = PosixTimeSpec::now_cpu_time();

            // to_string 的分配在锁外完成；push 返回的驱逐消息也必须
            // 在 KMSG 锁释放后再 drop，锁内只做无分配的槽位操作。
            let log_message = LogMessage::new(timestamp, log_level, message.to_string());

            let evicted = unsafe { KMSG.as_ref().unwrap().lock_irqsave().push(log_message) };
            drop(evicted);
        }
    }
}

/// 内核自定义日志器
///
/// todo: https://github.com/DragonOS-Community/DragonOS/issues/762
struct KernelLogger;

impl Log for KernelLogger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        // 根据全局日志级别过滤日志
        KERNEL_LOG_LEVEL.should_print(metadata.level().into())
    }

    fn log(&self, record: &log::Record) {
        if self.enabled(record.metadata()) {
            // 先输出到控制台再记录 kmsg：记录路径（锁+缓冲驱逐）一旦
            // 异常，现场仍然已经上了串口，不会被记录路径吞掉。
            Self::iodisplay(record);
            Self::kernel_log(record);
        }
    }

    fn flush(&self) {
        // 如果需要的话，可以在这里实现缓冲区刷新逻辑
    }
}

impl KernelLogger {
    fn iodisplay(record: &log::Record) {
        // Error/Warn 走关键同步通道：消息体不得被 tty 背压丢弃，否则
        // panic/IO 错误只剩级别前缀，现场无法排障。关键路径经
        // CriticalPrintkWriter 流式输出，不额外分配内存。
        if matches!(record.level(), Level::Error | Level::Warn) {
            let prefix = if matches!(record.level(), Level::Error) {
                "\x1B[41m[ ERROR ] \x1B[0m"
            } else {
                "\x1B[1;33m[ WARN ] \x1B[0m"
            };
            CriticalPrintkWriter.write_critical_string(prefix);
            let _ = CriticalPrintkWriter.write_fmt(format_args!(
                "({}:{})\t {}\n",
                record.file().unwrap_or(""),
                record.line().unwrap_or(0),
                record.args()
            ));
            return;
        }
        write!(PrintkWriter, "[ {} ] ", record.level(),).unwrap();
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
