use core::{
    fmt::{self, Write},
    sync::atomic::Ordering,
};

use alloc::string::ToString;
use log::{info, Log};

use super::lib_ui::textui::{textui_putstr, FontColor};

use crate::{
    driver::tty::{
        tty_driver::TtyOperation, tty_port::tty_port,
        virtual_terminal::virtual_console::CURRENT_VCNUM,
    },
    filesystem::procfs::{
        kmsg::KMSG,
        log::{LogLevel, LogMessage},
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

#[macro_export]
macro_rules! kdebug {
    ($($arg:tt)*) => {
        $crate::libs::printk::Logger.log(7,format_args!("({}:{})\t {}\n", file!(), line!(),format_args!($($arg)*)));
        $crate::libs::printk::PrintkWriter.__write_fmt(format_args!("[ DEBUG ] ({}:{})\t {}\n", file!(), line!(),format_args!($($arg)*)))
    }
}

#[macro_export]
macro_rules! kinfo {
    ($($arg:tt)*) => {
        $crate::libs::printk::Logger.log(6,format_args!("({}:{})\t {}\n", file!(), line!(),format_args!($($arg)*)));
        $crate::libs::printk::PrintkWriter.__write_fmt(format_args!("[ INFO ] ({}:{})\t {}\n", file!(), line!(),format_args!($($arg)*)))
    }
}

#[macro_export]
macro_rules! kwarn {
    ($($arg:tt)*) => {
        $crate::libs::printk::Logger.log(4,format_args!("({}:{})\t {}\n", file!(), line!(),format_args!($($arg)*)));
        $crate::libs::printk::PrintkWriter.__write_fmt(format_args!("\x1B[1;33m[ WARN ] \x1B[0m"));
        $crate::libs::printk::PrintkWriter.__write_fmt(format_args!("({}:{})\t {}\n", file!(), line!(),format_args!($($arg)*)));
    }
}

#[macro_export]
macro_rules! kerror {
    ($($arg:tt)*) => {
        $crate::libs::printk::Logger.log(3,format_args!("({}:{})\t {}\n", file!(), line!(),format_args!($($arg)*)));
        $crate::libs::printk::PrintkWriter.__write_fmt(format_args!("\x1B[41m[ ERROR ] \x1B[0m"));
        $crate::libs::printk::PrintkWriter.__write_fmt(format_args!("({}:{})\t {}\n", file!(), line!(),format_args!($($arg)*)));
    }
}

#[macro_export]
macro_rules! kBUG {
    ($($arg:tt)*) => {
        $crate::libs::printk::Logger.log(1,format_args!("({}:{})\t {}\n", file!(), line!(),format_args!($($arg)*)));
        $crate::libs::printk::PrintkWriter.__write_fmt(format_args!("\x1B[41m[ BUG ] \x1B[0m"));
        $crate::libs::printk::PrintkWriter.__write_fmt(format_args!("({}:{})\t {}\n", file!(), line!(),format_args!($($arg)*)));
    }
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
        let current_vcnum = CURRENT_VCNUM.load(Ordering::SeqCst);
        if current_vcnum != -1 {
            // tty已经初始化了之后才输出到屏幕
            let port = tty_port(current_vcnum as usize);
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

pub struct Logger;

impl Logger {
    pub fn log(&self, log_level: usize, message: fmt::Arguments) {
        if unsafe { KMSG.is_some() } {
            let timestamp: PosixTimeSpec = PosixTimeSpec::now_cpu_time();
            let log_level = LogLevel::from(log_level);

            let log_message = LogMessage::new(timestamp, log_level, message.to_string());

            unsafe { KMSG.as_ref().unwrap().lock_irqsave().push(log_message) };
        }
    }
}

/// 内核自定义日志器
///
/// todo: 完善他的功能，并且逐步把kinfo等宏，迁移到这个logger上面来。
struct CustomLogger;

impl Log for CustomLogger {
    fn enabled(&self, _metadata: &log::Metadata) -> bool {
        // 这里可以自定义日志过滤规则
        true
    }

    fn log(&self, record: &log::Record) {
        if self.enabled(record.metadata()) {
            // todo: 接入kmsg

            writeln!(
                PrintkWriter,
                "[ {} ] {} ({}:{}) {}",
                record.level(),
                record.target(),
                record.file().unwrap_or(""),
                record.line().unwrap_or(0),
                record.args()
            )
            .unwrap();
        }
    }

    fn flush(&self) {
        // 如果需要的话，可以在这里实现缓冲区刷新逻辑
    }
}

pub fn early_init_logging() {
    log::set_logger(&CustomLogger).unwrap();
    log::set_max_level(log::LevelFilter::Debug);
    info!("Logging initialized");
}
