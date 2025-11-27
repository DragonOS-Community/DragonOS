//! 内核日志级别管理模块
//!
//! 提供类似Linux内核的loglevel功能，支持通过cmdline参数和procfs接口
//! 动态控制内核日志输出级别。

use core::sync::atomic::{AtomicU8, Ordering};

use system_error::SystemError;

use crate::init::cmdline::{KernelCmdlineKV, KernelCmdlineParameter, KCMDLINE_PARAM_KV};

/// 全局内核日志级别配置
///
/// 遵循Linux内核语义：
/// - console_loglevel: 控制台输出级别，只有级别小于等于此值的消息才会输出
/// - default_message_loglevel: 默认消息级别
/// - minimum_console_loglevel: 最小控制台级别
/// - default_console_loglevel: 默认控制台级别
pub static KERNEL_LOG_LEVEL: KernelLogLevel = KernelLogLevel::new();

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

impl From<::log::Level> for LogLevel {
    fn from(value: ::log::Level) -> Self {
        match value {
            log::Level::Error => LogLevel::ERR,   // 3
            log::Level::Warn => LogLevel::WARN,   // 4
            log::Level::Info => LogLevel::INFO,   // 6
            log::Level::Debug => LogLevel::DEBUG, // 7
            log::Level::Trace => LogLevel::DEBUG, // 7 (maps to DEBUG)
        }
    }
}

/// 内核日志级别管理结构
#[derive(Debug)]
pub struct KernelLogLevel {
    /// 控制台输出级别 (console_loglevel)
    /// 只有级别小于等于此值的消息才会输出到控制台
    pub console_level: AtomicU8,
    /// 默认消息级别 (default_message_loglevel)
    pub default_message_level: AtomicU8,
    /// 最小控制台级别 (minimum_console_loglevel)
    pub minimum_level: AtomicU8,
    /// 默认控制台级别 (default_console_loglevel)
    pub default_console_level: AtomicU8,
}

impl KernelLogLevel {
    /// 创建新的内核日志级别配置
    ///
    /// 默认值遵循Linux内核标准：
    /// - console_loglevel: 7 (DEBUG)
    /// - default_message_loglevel: 4 (WARNING)
    /// - minimum_console_loglevel: 1 (ALERT)
    /// - default_console_loglevel: 7 (DEBUG)
    pub const fn new() -> Self {
        Self {
            console_level: AtomicU8::new(7),         // DEBUG级别
            default_message_level: AtomicU8::new(4), // WARNING级别
            minimum_level: AtomicU8::new(1),         // ALERT级别
            default_console_level: AtomicU8::new(7), // DEBUG级别
        }
    }

    /// 检查消息是否应该被输出到控制台
    ///
    /// # 参数
    /// - `message_level`: 消息级别 (0-7)
    ///
    /// # 返回值
    /// - `true`: 消息级别 <= 控制台级别，应该输出
    /// - `false`: 消息级别 > 控制台级别，应该过滤
    ///
    /// # Linux语义
    /// 遵循Linux内核的过滤规则：message_level <= console_loglevel
    pub fn should_print(&self, message_level: LogLevel) -> bool {
        let console_level = self.console_level.load(Ordering::Acquire);
        // Linux语义：message_level <= console_loglevel 时输出
        message_level as u8 <= console_level
    }

    /// 设置控制台日志级别
    ///
    /// # 参数
    /// - `level`: 新的控制台级别 (0-7)
    ///
    /// # 返回值
    /// - `Ok(())`: 设置成功
    /// - `Err(SystemError::EINVAL)`: 级别值无效
    pub fn set_console_level(&self, level: u8) -> Result<(), SystemError> {
        if level <= 7 {
            self.console_level.store(level, Ordering::Release);
            Ok(())
        } else {
            Err(SystemError::EINVAL)
        }
    }

    /// 获取当前控制台日志级别
    pub fn get_console_level(&self) -> u8 {
        self.console_level.load(Ordering::Acquire)
    }

    /// 获取默认消息级别
    pub fn get_default_message_level(&self) -> u8 {
        self.default_message_level.load(Ordering::Acquire)
    }

    /// 获取最小控制台级别
    pub fn get_minimum_level(&self) -> u8 {
        self.minimum_level.load(Ordering::Acquire)
    }

    /// 获取默认控制台级别
    pub fn get_default_console_level(&self) -> u8 {
        self.default_console_level.load(Ordering::Acquire)
    }

    /// 获取所有日志级别配置
    ///
    /// # 返回值
    /// 返回包含四个级别值的数组：[console, default_message, minimum, default_console]
    pub fn get_all_levels(&self) -> [u8; 4] {
        [
            self.get_console_level(),
            self.get_default_message_level(),
            self.get_minimum_level(),
            self.get_default_console_level(),
        ]
    }
}

/// loglevel命令行参数处理
///
/// 支持格式：loglevel=N (N=0-7)
/// 示例：loglevel=4 只输出WARNING及以上级别的日志
#[linkme::distributed_slice(KCMDLINE_PARAM_KV)]
static LOGLEVEL_PARAM: KernelCmdlineParameter = KernelCmdlineParameter::KV(KernelCmdlineKV {
    name: "loglevel",
    value: None,
    initialized: false,
    default: "7", // 默认DEBUG级别
});

/// 处理loglevel参数
///
/// 在cmdline参数解析完成后调用，设置全局日志级别
pub fn handle_loglevel_param() {
    if let Some(param) = KCMDLINE_PARAM_KV.iter().find(|x| x.name() == "loglevel") {
        if let Some(value_str) = param.value_str() {
            if let Ok(level) = value_str.parse::<u8>() {
                if level <= 7 {
                    let _ = KERNEL_LOG_LEVEL.set_console_level(level);
                    log::info!("loglevel: set console log level to {} via cmdline", level);
                } else {
                    log::warn!("loglevel: invalid level {}, must be 0-7", level);
                }
            } else {
                log::warn!("loglevel: invalid value '{}', must be a number", value_str);
            }
        }
    }
}
