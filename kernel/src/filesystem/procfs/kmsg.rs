use super::log_message::LogMessage;
use alloc::{borrow::ToOwned, string::ToString, vec::Vec};
use kdepends::thingbuf::ThingBuf;
use system_error::SystemError;

const CONSOLE_LOGLEVEL_DEFAULT: usize = 8;

/// 日志
pub struct Kmsg {
    /// 环形缓冲区
    buffer: ThingBuf<LogMessage>,
    /// 缓冲区字节数组
    data: Vec<u8>,
    /// 能够输出到控制台的日志级别
    console_loglevel: usize,
}

impl Kmsg {
    pub fn new(capacity: usize) -> Self {
        Kmsg {
            buffer: ThingBuf::<LogMessage>::new(capacity),
            data: Vec::new(),
            console_loglevel: CONSOLE_LOGLEVEL_DEFAULT,
        }
    }

    /// 添加日志消息
    pub fn push(&mut self, msg: LogMessage) -> Result<(), SystemError> {
        let len = self.buffer.len();
        if len == self.buffer.capacity() {
            self.buffer.pop();
        }

        if self.buffer.push(msg).is_err() {
            return Err(SystemError::ENOMEM);
        }

        return Ok(());
    }

    /// 读取缓冲区
    pub fn read(&mut self, len: usize, buf: &mut [u8]) -> Result<usize, SystemError> {
        self.tobytes()?;

        let mut len = len;
        len = self.data.len().min(len);

        // buffer空间不足
        if buf.len() < len {
            return Err(SystemError::ENOBUFS);
        }

        // 拷贝数据
        let src = &self.data[0..len];
        buf[0..src.len()].copy_from_slice(src);

        self.console_loglevel = CONSOLE_LOGLEVEL_DEFAULT;

        return Ok(src.len());
    }

    /// 清空缓冲区
    pub fn clear(&mut self) -> Result<usize, SystemError> {
        while !self.buffer.is_empty() {
            self.buffer.pop();
        }
        self.data.clear();

        return Ok(0);
    }

    /// 读取并清空缓冲区
    pub fn read_clear(&mut self, len: usize, buf: &mut [u8]) -> Result<usize, SystemError> {
        let r = self.read(len, buf);
        self.clear()?;

        return r;
    }

    /// 设置输出到控制台的日志级别
    pub fn set_level(&mut self, log_level: usize) -> Result<usize, SystemError> {
        let log_level = log_level - 1;
        if log_level > 8 {
            return Err(SystemError::EINVAL);
        }

        self.console_loglevel = log_level;

        return Ok(0);
    }

    // 将环形缓冲区的日志消息转成字节数组以拷入用户buf
    pub fn tobytes(&mut self) -> Result<usize, SystemError> {
        self.data.clear();

        let size = self.buffer.len();

        // 如果控制台日志级别为默认级别，则可以输出所有日志消息到控制台
        if self.console_loglevel == CONSOLE_LOGLEVEL_DEFAULT {
            for _ in 0..size {
                if let Some(msg) = self.buffer.pop() {
                    self.data.append(&mut msg.to_string().as_bytes().to_owned());
                    self.push(msg)?;
                }
            }
        }
        // 否则，只能输出特定日志级别的日志消息到控制台
        else {
            for _ in 0..size {
                if let Some(msg) = self.buffer.pop() {
                    if msg.level() as usize != self.console_loglevel {
                        self.push(msg)?;
                        continue;
                    }
                    self.data.append(&mut msg.to_string().as_bytes().to_owned());
                    self.push(msg)?;
                }
            }
        }

        return Ok(self.data.len());
    }

    // 返回内核缓冲区所占字节数
    pub fn buffer_size(&mut self) -> Result<usize, SystemError> {
        self.tobytes()?;
        return Ok(self.data.len());
    }
}
