use core::sync::atomic::{compiler_fence, Ordering};

use super::log::{LogLevel, LogMessage};

use crate::libs::spinlock::SpinLock;

use alloc::{borrow::ToOwned, string::ToString, vec::Vec};

use kdepends::ringbuffer::{AllocRingBuffer, RingBuffer};

use log::info;
use system_error::SystemError;

/// 缓冲区容量
const KMSG_BUFFER_CAPACITY: usize = 1024;

/// 全局环形缓冲区
pub static mut KMSG: Option<SpinLock<Kmsg>> = None;

/// 初始化KMSG
pub fn kmsg_init() {
    info!("kmsg_init");
    let kmsg = SpinLock::new(Kmsg::new());

    compiler_fence(Ordering::SeqCst);
    unsafe { KMSG = Some(kmsg) };
    compiler_fence(Ordering::SeqCst);
    info!("kmsg_init done");
}

/// 日志
pub struct Kmsg {
    /// 环形缓冲区
    buffer: AllocRingBuffer<LogMessage>,
    /// 缓冲区字节数组
    data: Vec<u8>,
    /// 能够输出到控制台的日志级别，当console_loglevel为DEFAULT时，表示可以打印所有级别的日志消息到控制台
    console_loglevel: LogLevel,
    /// 判断buffer在上一次转成字节数组之后是否发生变动
    is_changed: bool,
}

impl Kmsg {
    pub fn new() -> Self {
        Kmsg {
            buffer: AllocRingBuffer::new(KMSG_BUFFER_CAPACITY),
            data: Vec::new(),
            console_loglevel: LogLevel::DEFAULT,
            is_changed: false,
        }
    }

    /// 添加日志消息
    pub fn push(&mut self, msg: LogMessage) {
        self.buffer.push(msg);
        self.is_changed = true;
    }

    /// 读取缓冲区
    pub fn read(&mut self, buf: &mut [u8]) -> Result<usize, SystemError> {
        self.tobytes();

        match self.console_loglevel {
            LogLevel::DEFAULT => self.read_all(buf),
            _ => self.read_level(buf),
        }
    }

    /// 读取缓冲区所有日志消息
    fn read_all(&mut self, buf: &mut [u8]) -> Result<usize, SystemError> {
        let len = self.data.len().min(buf.len());

        // 拷贝数据
        let src = &self.data[0..len];
        buf[0..len].copy_from_slice(src);

        return Ok(len);
    }

    /// 读取缓冲区特定level的日志消息
    fn read_level(&mut self, buf: &mut [u8]) -> Result<usize, SystemError> {
        let mut data_level: Vec<u8> = Vec::new();

        for msg in self.buffer.iter() {
            if msg.level() == self.console_loglevel {
                data_level.append(&mut msg.to_string().as_bytes().to_owned());
            }
        }

        let len = data_level.len().min(buf.len());

        // 拷贝数据
        let src = &data_level[0..len];
        buf[0..len].copy_from_slice(src);

        // 将控制台输出日志level改回默认，否则之后都是打印特定level的日志消息
        self.console_loglevel = LogLevel::DEFAULT;

        return Ok(data_level.len());
    }

    /// 读取并清空缓冲区
    pub fn read_clear(&mut self, buf: &mut [u8]) -> Result<usize, SystemError> {
        let r = self.read_all(buf);
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

        self.console_loglevel = match log_level {
            0 => LogLevel::EMERG,
            1 => LogLevel::ALERT,
            2 => LogLevel::CRIT,
            3 => LogLevel::ERR,
            4 => LogLevel::WARN,
            5 => LogLevel::NOTICE,
            6 => LogLevel::INFO,
            7 => LogLevel::DEBUG,
            8 => LogLevel::DEFAULT,
            _ => return Err(SystemError::EINVAL),
        };

        return Ok(0);
    }

    /// 将环形缓冲区的日志消息转成字节数组以拷入用户buf
    fn tobytes(&mut self) -> usize {
        if self.is_changed {
            self.data.clear();

            if self.console_loglevel == LogLevel::DEFAULT {
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
