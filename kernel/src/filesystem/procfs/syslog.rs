use system_error::SystemError;

use crate::syscall::Syscall;

use super::KMSG;

/// Close the log.  Currently a NOP.
pub const SYSLOG_ACTION_CLOSE: usize = 0;

/// Open the log. Currently a NOP.
pub const SYSLOG_ACTION_OPEN: usize = 1;

/// Read from the log.
pub const SYSLOG_ACTION_READ: usize = 2;

/// Read and clear all messages remaining in the ring buffer.
pub const SYSLOG_ACTION_READ_CLEAR: usize = 4;

/// Clear ring buffer.
pub const SYSLOG_ACTION_CLEAR: usize = 5;

/// Set level of messages printed to console.
pub const SYSLOG_ACTION_CONSOLE_LEVEL: usize = 8;

/// Return size of the log buffer.
pub const SYSLOG_ACTION_SIZE_BUFFER: usize = 10;

impl Syscall {
    pub fn do_syslog(
        syslog_action_type: usize,
        buf: &mut [u8],
        len: usize,
    ) -> Result<usize, SystemError> {
        match syslog_action_type {
            SYSLOG_ACTION_CLOSE => Ok(0),
            SYSLOG_ACTION_OPEN => Ok(0),
            SYSLOG_ACTION_READ => KMSG.lock().read(len, buf),
            SYSLOG_ACTION_READ_CLEAR => KMSG.lock().read_clear(len, buf),
            SYSLOG_ACTION_CLEAR => KMSG.lock().clear(),
            SYSLOG_ACTION_SIZE_BUFFER => KMSG.lock().buffer_size(),
            SYSLOG_ACTION_CONSOLE_LEVEL => KMSG.lock().set_level(len),
            _ => todo!(),
        }
    }
}
