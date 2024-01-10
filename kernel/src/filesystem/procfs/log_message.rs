use core::fmt::{Display, Formatter, Result};

use alloc::string::String;

/// 日志类型
#[derive(Default, Clone)]
pub enum LogType {
    /// 启动信息
    Startup,
    /// 驱动信息
    Driver,
    /// 系统信息
    System,
    /// 硬件信息
    Hardware,
    /// 内核模块信息
    KernelModule,
    /// 内核调试信息
    KernelDebug,
    #[default]
    Default,
}

/// 日志级别
#[derive(Default, Clone)]
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
    DEFAULT,
}

/// 日志消息
#[derive(Default, Clone)]
pub struct LogMessage {
    /// 时间戳
    timestamp: String,
    /// 日志级别
    level: LogLevel,
    /// 日志类型
    log_type: LogType,
    /// 日志消息
    message: String,
}

impl LogMessage {
    pub fn new(timestamp: String, level: LogLevel, log_type: LogType, message: String) -> Self {
        LogMessage {
            timestamp,
            level,
            log_type,
            message,
        }
    }

    pub fn level(&self) -> LogLevel {
        self.level.clone()
    }

    pub fn timestamp(&self) -> String {
        self.timestamp.clone()
    }

    pub fn log_type(&self) -> LogType {
        self.log_type.clone()
    }

    pub fn message(&self) -> String {
        self.message.clone()
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

        let log_type = match self.log_type {
            LogType::Startup => "Startup",
            LogType::Driver => "Driver",
            LogType::System => "System",
            LogType::Hardware => "Hardware",
            LogType::KernelModule => "KernelModule",
            LogType::KernelDebug => "KernelDebug",
            LogType::Default => "Default",
        };

        let message = &self.message;

        let res = format!("<{}>[{}] ({}): {}\n", level, timestamp, log_type, message);
        return write!(f, "{}", res);
    }
}
