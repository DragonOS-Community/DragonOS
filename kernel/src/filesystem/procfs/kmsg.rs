use super::log_message::LogMessage;
use alloc::{borrow::ToOwned, string::ToString, vec::Vec};
use kdepends::ringbuffer::{ConstGenericRingBuffer, RingBuffer};
use system_error::SystemError;

/// 缓冲区容量
const KMSG_BUFFER_CAPACITY: usize = 1024;

/// 当KMSG的console_loglevel等于CONSOLE_LOGLEVEL_DEFAULT时，表示可以打印所有级别的日志消息到控制台
const CONSOLE_LOGLEVEL_DEFAULT: usize = 8;

/// 日志
pub struct Kmsg {
    /// 环形缓冲区
    buffer: ConstGenericRingBuffer<LogMessage, KMSG_BUFFER_CAPACITY>,
    /// 缓冲区字节数组
    data: Vec<u8>,
    /// 能够输出到控制台的日志级别
    console_loglevel: usize,
    /// 判断buffer在上一次转成字节数组之后是否发生变动
    is_changed: bool,
}

impl Kmsg {
    pub fn new() -> Self {
        Kmsg {
            buffer: ConstGenericRingBuffer::<LogMessage, KMSG_BUFFER_CAPACITY>::new(),
            data: Vec::new(),
            console_loglevel: CONSOLE_LOGLEVEL_DEFAULT,
            is_changed: false,
        }
    }

    /// 添加日志消息
    pub fn push(&mut self, msg: LogMessage) {
        self.buffer.push(msg);
        self.is_changed = true;
    }

    /// 读取缓冲区
    pub fn read(&mut self, len: usize, buf: &mut [u8]) -> Result<usize, SystemError> {
        let r = match self.console_loglevel {
            CONSOLE_LOGLEVEL_DEFAULT => self.read_all(len, buf),
            _ => self.read_level(len, buf),
        };

        r
    }

    /// 读取缓冲区所有日志消息
    fn read_all(&mut self, len: usize, buf: &mut [u8]) -> Result<usize, SystemError> {
        self.tobytes();

        let mut len = len;
        len = self.data.len().min(len);

        // buffer空间不足
        if buf.len() < len {
            return Err(SystemError::ENOBUFS);
        }

        // 拷贝数据
        let src = &self.data[0..len];
        buf[0..src.len()].copy_from_slice(src);

        return Ok(src.len());
    }

    /// 读取缓冲区特定level的日志消息
    fn read_level(&mut self, len: usize, buf: &mut [u8]) -> Result<usize, SystemError> {
        let mut data_level: Vec<u8> = Vec::new();

        for msg in self.buffer.iter() {
            if msg.level() as usize == self.console_loglevel {
                data_level.append(&mut msg.to_string().as_bytes().to_owned());
            }
        }

        let mut len = len;
        len = data_level.len().min(len);

        // buffer空间不足
        if buf.len() < len {
            return Err(SystemError::ENOBUFS);
        }

        // 拷贝数据
        let src = &data_level[0..len];
        buf[0..src.len()].copy_from_slice(src);

        // 将控制台输出日志level改回默认，否则之后都是打印特定level的日志消息
        self.console_loglevel = CONSOLE_LOGLEVEL_DEFAULT;

        return Ok(data_level.len());
    }

    /// 读取并清空缓冲区
    pub fn read_clear(&mut self, len: usize, buf: &mut [u8]) -> Result<usize, SystemError> {
        let r = self.read_all(len, buf);
        self.clear()?;

        return r;
    }

    /// 清空缓冲区
    pub fn clear(&mut self) -> Result<usize, SystemError> {
        self.buffer.clear();
        self.data.clear();

        return Ok(0);
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

    /// 将环形缓冲区的日志消息转成字节数组以拷入用户buf
    fn tobytes(&mut self) -> usize {
        if self.is_changed {
            self.data.clear();

            if self.console_loglevel == CONSOLE_LOGLEVEL_DEFAULT {
                for msg in self.buffer.iter() {
                    self.data.append(&mut msg.to_string().as_bytes().to_owned());
                }
            }

            self.is_changed = false;
        }

        return self.data.len();
    }

    // 返回内核缓冲区所占字节数
    pub fn data_size(&mut self) -> Result<usize, SystemError> {
        return Ok(self.tobytes());
    }
}
