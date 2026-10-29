use system_error::SystemError;

use crate::syscall::Syscall;

use super::kmsg::{check_syslog_permission, kmsg, read_stream, LogSnapshot, KMSG_TEXT_CAPACITY};
use crate::{
    mm::{access_ok, VirtAddr},
    syscall::user_access::copy_to_user_protected,
};

/// 操作内核环形缓冲区
enum SyslogAction {
    /// Close the log.  Currently a NOP.
    Close = 0,
    /// Open the log. Currently a NOP.
    Open = 1,
    /// Read from the log.
    Read = 2,
    ///  Read all messages from the ring buffer.
    ReadAll = 3,
    /// Read and clear all messages remaining in the ring buffer.
    ReadClear = 4,
    /// Clear ring buffer.
    Clear = 5,
    ConsoleOff = 6,
    ConsoleOn = 7,
    SizeUnread = 9,
    /// Set level of messages printed to console.
    ConsoleLevel = 8,
    /// Return size of the log buffer.
    SizeBuffer = 10,
    /// Invalid SyslogAction
    Inval,
}

impl From<i32> for SyslogAction {
    fn from(value: i32) -> Self {
        match value {
            0 => SyslogAction::Close,
            1 => SyslogAction::Open,
            2 => SyslogAction::Read,
            3 => SyslogAction::ReadAll,
            4 => SyslogAction::ReadClear,
            5 => SyslogAction::Clear,
            6 => SyslogAction::ConsoleOff,
            7 => SyslogAction::ConsoleOn,
            8 => SyslogAction::ConsoleLevel,
            9 => SyslogAction::SizeUnread,
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
        syslog_action_type: i32,
        buf: usize,
        len: i32,
        from_user: bool,
    ) -> Result<usize, SystemError> {
        check_syslog_permission(syslog_action_type)?;
        let action = SyslogAction::from(syslog_action_type);
        let kmsg = kmsg()?;
        match action {
            SyslogAction::Close | SyslogAction::Open => return Ok(0),
            SyslogAction::Clear => {
                kmsg.lock_irqsave().clear();
                return Ok(0);
            }
            SyslogAction::ConsoleOff => {
                kmsg.lock_irqsave().console_off();
                return Ok(0);
            }
            SyslogAction::ConsoleOn => {
                kmsg.lock_irqsave().console_on();
                return Ok(0);
            }
            SyslogAction::ConsoleLevel => return kmsg.lock_irqsave().set_level(len),
            SyslogAction::SizeUnread => return Ok(kmsg.lock_irqsave().unread_bytes()),
            SyslogAction::Inval => return Err(SystemError::EINVAL),
            SyslogAction::SizeBuffer => return Ok(KMSG_TEXT_CAPACITY),
            _ => {}
        }
        if buf == 0 || len < 0 {
            return Err(SystemError::EINVAL);
        }
        if len == 0 {
            return Ok(0);
        }
        let len = len as usize;
        if from_user && access_ok(VirtAddr::new(buf), len).is_err() {
            return Err(SystemError::EFAULT);
        }
        let mut offset = 0;
        let mut copy = |bytes: &[u8]| {
            let address = buf.checked_add(offset).ok_or(SystemError::EFAULT)?;
            if from_user {
                // No ring lock is held: prefault and protected copy may sleep.
                unsafe {
                    copy_to_user_protected(VirtAddr::new(address), bytes)?;
                }
            } else {
                // Kernel callers must supply a valid writable output range.
                unsafe {
                    core::ptr::copy_nonoverlapping(bytes.as_ptr(), address as *mut u8, bytes.len());
                }
            }
            offset += bytes.len();
            Ok(())
        };
        if matches!(action, SyslogAction::Read) {
            return read_stream(len, &mut copy);
        }
        let mut snapshot = LogSnapshot::try_new()?;
        kmsg.lock_irqsave().capture(&mut snapshot);
        let (result, cutoff) = snapshot.read_all(len, &mut copy);
        if matches!(action, SyslogAction::ReadClear) {
            // Commit only complete records, including on a later copy fault.
            // max prevents an older concurrent reader undoing a newer clear.
            kmsg.lock_irqsave().clear_through(cutoff);
        }
        result
    }
}
