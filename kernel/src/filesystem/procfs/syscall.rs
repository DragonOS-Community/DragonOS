use core::usize;

use system_error::SystemError;

use crate::syscall::Syscall;

use super::KMSG;

/// 操作内核环形缓冲区
enum SyslogAction {
    /// Close the log.  Currently a NOP.
    SyslogActionClose = 0,
    /// Open the log. Currently a NOP.
    SyslogActionOpen = 1,
    /// Read from the log.
    SyslogActionRead = 2,
    /// Read and clear all messages remaining in the ring buffer.
    SyslogActionReadClear = 4,
    /// Clear ring buffer.
    SyslogActionClear = 5,
    /// Set level of messages printed to console.
    SyslogActionConsoleLevel = 8,
    /// Return size of the log buffer.
    SyslogActionSizeBuffer = 10,
}

impl Syscall {
    /// # 操作内核环形缓冲区
    ///
    /// ## 参数
    /// - syslog_action_type: 操作码
    /// - buf：用户缓冲区
    /// - len: 需要从内核环形缓冲区读取的字节数。如果操作码为8，即SyslogActionConsoleLevel，则len为待设置的日志级别
    ///
    /// ## 返回值
    /// - 成功，Ok(usize)
    /// - 失败，Err(SystemError) 操作失败，返回posix错误码
    ///

    pub fn do_syslog(
        syslog_action_type: usize,
        buf: &mut [u8],
        len: usize,
    ) -> Result<usize, SystemError> {
        let syslog_action = match syslog_action_type {
            0 => SyslogAction::SyslogActionClose,
            1 => SyslogAction::SyslogActionOpen,
            2 => SyslogAction::SyslogActionRead,
            4 => SyslogAction::SyslogActionReadClear,
            5 => SyslogAction::SyslogActionClear,
            8 => SyslogAction::SyslogActionConsoleLevel,
            10 => SyslogAction::SyslogActionSizeBuffer,
            _ => return Err(SystemError::EINVAL),
        };

        let mut kmsg_guard = KMSG.lock();
        let r = match syslog_action {
            SyslogAction::SyslogActionClose => Ok(0),
            SyslogAction::SyslogActionOpen => Ok(0),
            SyslogAction::SyslogActionRead => kmsg_guard.read(len, buf),
            SyslogAction::SyslogActionReadClear => kmsg_guard.read_clear(len, buf),
            SyslogAction::SyslogActionClear => kmsg_guard.clear(),
            SyslogAction::SyslogActionSizeBuffer => kmsg_guard.data_size(),
            SyslogAction::SyslogActionConsoleLevel => kmsg_guard.set_level(len),
        };
        drop(kmsg_guard);

        return r;
    }
}
