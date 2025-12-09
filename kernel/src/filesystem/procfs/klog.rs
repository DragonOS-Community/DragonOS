use core::fmt::{Display, Formatter, Result};

use alloc::string::String;

use crate::{debug::klog::loglevel::LogLevel, time::PosixTimeSpec};

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
