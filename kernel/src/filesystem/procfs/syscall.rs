use system_error::SystemError;

use crate::syscall::Syscall;

use super::kmsg::KMSG;

/// 操作内核环形缓冲区
enum SyslogAction {
    /// Close the log.  Currently a NOP.
    Close = 0,
    /// Open the log. Currently a NOP.
    Open = 1,
    /// Read from the log.
    Read = 2,
    /// Read and clear all messages remaining in the ring buffer.
    ReadClear = 4,
    /// Clear ring buffer.
    Clear = 5,
    /// Set level of messages printed to console.
    ConsoleLevel = 8,
    /// Return size of the log buffer.
    SizeBuffer = 10,
    /// Invalid SyslogAction
    Inval,
}

impl From<usize> for SyslogAction {
    fn from(value: usize) -> Self {
        match value {
            0 => SyslogAction::Close,
            1 => SyslogAction::Open,
            2 => SyslogAction::Read,
            4 => SyslogAction::ReadClear,
            5 => SyslogAction::Clear,
            8 => SyslogAction::ConsoleLevel,
            10 => SyslogAction::SizeBuffer,
            _ => SyslogAction::Inval,
        }
    }
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
        let syslog_action = SyslogAction::from(syslog_action_type);

        let mut kmsg_guard = unsafe { KMSG.as_ref().unwrap().lock_irqsave() };

        match syslog_action {
            SyslogAction::Close => Ok(0),
            SyslogAction::Open => Ok(0),
            SyslogAction::Read => kmsg_guard.read(buf),
            SyslogAction::ReadClear => kmsg_guard.read_clear(buf),
            SyslogAction::Clear => kmsg_guard.clear(),
            SyslogAction::SizeBuffer => kmsg_guard.data_size(),
            SyslogAction::ConsoleLevel => kmsg_guard.set_level(len),
            SyslogAction::Inval => return Err(SystemError::EINVAL),
        }
    }
}
