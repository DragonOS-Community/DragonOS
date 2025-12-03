//! Procfs sysctl接口实现
//!
//! 提供类似Linux的/proc/sys接口，支持动态配置内核参数

use alloc::{string::String, vec::Vec};
use system_error::SystemError;

use crate::debug::klog::loglevel::KERNEL_LOG_LEVEL;

/// 内核日志级别配置管理器
///
/// 提供读取和写入内核日志级别配置的功能
/// 格式：console_loglevel\tdefault_message_loglevel\tminimum_console_loglevel\tdefault_console_loglevel
#[derive(Debug)]
pub struct PrintkSysctl;

impl PrintkSysctl {
    /// 读取当前的内核日志级别配置
    ///
    /// 返回格式："console_loglevel\tdefault_message_loglevel\tminimum_console_loglevel\tdefault_console_loglevel\n"
    fn read_config(&self) -> Result<String, SystemError> {
        let levels = KERNEL_LOG_LEVEL.get_all_levels();
        Ok(format!(
            "{}\t{}\t{}\t{}\n",
            levels[0], levels[1], levels[2], levels[3]
        ))
    }

    /// 写入内核日志级别配置
    ///
    /// 支持写入单个值或多个值，但只处理第一个值（控制台日志级别）
    /// 输入数据格式："console_loglevel" 或 "console_loglevel default_message_loglevel minimum_console_loglevel default_console_loglevel"
    fn write_config(&self, data: &[u8]) -> Result<usize, SystemError> {
        let input = core::str::from_utf8(data).map_err(|_| SystemError::EINVAL)?;
        let parts: Vec<&str> = input.split_whitespace().collect();

        if parts.is_empty() {
            return Err(SystemError::EINVAL);
        }

        // 只处理第一个值（控制台日志级别）
        if let Ok(level) = parts[0].parse::<u8>() {
            KERNEL_LOG_LEVEL.set_console_level(level)?;
            log::info!(
                "sysctl: set console log level to {} via /proc/sys/kernel/printk",
                level
            );
            Ok(data.len())
        } else {
            log::warn!("sysctl: invalid loglevel value '{}'", parts[0]);
            Err(SystemError::EINVAL)
        }
    }

    /// 读取配置到缓冲区（用于文件系统读取操作）
    pub fn read_to_buffer(&self, buf: &mut [u8]) -> Result<usize, SystemError> {
        let content = self.read_config()?;
        let content_bytes = content.as_bytes();

        let read_len = buf.len().min(content_bytes.len());
        buf[..read_len].copy_from_slice(&content_bytes[..read_len]);

        Ok(read_len)
    }

    /// 从缓冲区写入配置（用于文件系统写入操作）
    pub fn write_from_buffer(&self, buf: &[u8]) -> Result<usize, SystemError> {
        self.write_config(buf)
    }
}
