use core::fmt::{Display, Formatter, Result};

use alloc::string::String;

use crate::time::PosixTimeSpec;

// /// 日志类型
// #[derive(Default, Clone, Debug)]
// pub enum LogType {
//     /// 启动信息
//     Startup,
//     /// 驱动信息
//     Driver,
//     /// 系统信息
//     System,
//     /// 硬件信息
//     Hardware,
//     /// 内核模块信息
//     KernelModule,
//     /// 内核调试信息
//     KernelDebug,
//     #[default]
//     Default,
// }

/// 日志级别
#[derive(Default, Clone, PartialEq, Debug)]
pub enum LogLevel {
    EMERG = 0,
    ALERT = 1,
    CRIT = 2,
    ERR = 3,
    WARN = 4,
    NOTICE = 5,
    INFO = 6,
    DEBUG = 7,
    #[default]
    DEFAULT = 8,
}

impl From<usize> for LogLevel {
    fn from(value: usize) -> Self {
        match value {
            0 => LogLevel::EMERG,
            1 => LogLevel::ALERT,
            2 => LogLevel::CRIT,
            3 => LogLevel::ERR,
            4 => LogLevel::WARN,
            5 => LogLevel::NOTICE,
            6 => LogLevel::INFO,
            7 => LogLevel::DEBUG,
            _ => LogLevel::DEFAULT,
        }
    }
}

/// 日志消息
#[derive(Default, Clone, Debug)]
pub struct LogMessage {
    /// 时间戳
    timestamp: PosixTimeSpec,
    /// 日志级别
    level: LogLevel,
    // /// 日志类型
    // log_type: LogType,
    /// 日志消息
    message: String,
}

impl LogMessage {
    pub fn new(timestamp: PosixTimeSpec, level: LogLevel, message: String) -> Self {
        LogMessage {
            timestamp,
            level,
            message,
        }
    }

    pub fn level(&self) -> LogLevel {
        self.level.clone()
    }
}

impl Display for LogMessage {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result {
        let timestamp = &self.timestamp;
        let level = match self.level {
            LogLevel::EMERG => "EMERG",
            LogLevel::ALERT => "ALERT",
            LogLevel::CRIT => "CRIT",
            LogLevel::ERR => "ERR",
            LogLevel::WARN => "WARNING",
            LogLevel::NOTICE => "NOTICE",
            LogLevel::INFO => "INFO",
            LogLevel::DEBUG => "DEBUG",
            LogLevel::DEFAULT => "Default",
        };

        let message = &self.message;

        let res = format!(
            "<{}>[{}.{}] : {}\n",
            level, timestamp.tv_sec, timestamp.tv_nsec, message
        );
        return write!(f, "{}", res);
    }
}
