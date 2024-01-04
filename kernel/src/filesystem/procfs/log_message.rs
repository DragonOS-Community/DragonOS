use alloc::string::{String, ToString};

// 日志级别
#[derive(Default, Clone)]
pub enum LogLevel {
    Emergency,
    Alert,
    Critical,
    Error,
    Warn,
    Notice,
    Info,
    Debug,
    #[default]
    Default,
}

impl LogLevel {
    /// 将日志级别转成字符串形式
    pub fn level2str(level: LogLevel) -> String {
        let level_str = match level {
            LogLevel::Emergency => "Emergency",
            LogLevel::Alert => "Alert",
            LogLevel::Critical => "Critical",
            LogLevel::Error => "Error",
            LogLevel::Warn => "Warn",
            LogLevel::Notice => "Notice",
            LogLevel::Info => "Info",
            LogLevel::Debug => "Debug",
            LogLevel::Default => "Default",
        };

        return level_str.to_string();
    }
}

/// 日志消息
#[derive(Default, Clone)]
pub struct LogMessage {
    level: LogLevel,
    time: String,
    message: String,
}

impl LogMessage {
    pub fn new(level: LogLevel, time: String, message: String) -> Self {
        LogMessage {
            level,
            time,
            message,
        }
    }

    pub fn level(&self) -> LogLevel {
        self.level.clone()
    }

    pub fn time(&self) -> String {
        self.time.clone()
    }

    pub fn message(&self) -> String {
        self.message.clone()
    }
}
