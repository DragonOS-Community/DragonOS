use core::mem::size_of;

use super::log_message::{LogLevel, LogMessage};
use alloc::{borrow::ToOwned, vec::Vec};
use kdepends::thingbuf::ThingBuf;
use system_error::SystemError;

/// 日志
pub struct Kmsg {
    /// 环形缓冲区
    buffer: ThingBuf<LogMessage>,
    data: Vec<u8>,
}

impl Kmsg {
    pub fn new(capacity: usize) -> Self {
        Kmsg {
            buffer: ThingBuf::<LogMessage>::new(capacity),
            data: Vec::new(),
        }
    }

    /// 添加日志消息
    pub fn push(&mut self, msg: LogMessage) {
        let len = self.buffer.len();
        if len == self.buffer.capacity() {
            self.buffer.pop();
        }
        let _ = self.buffer.push(msg);
    }

    /// 打开kmsg文件
    pub fn open(&mut self) -> Result<i64, SystemError> {
        self.tobytes();

        self.data.retain(|x| *x != 0);
        self.data.push(0);

        return Ok((self.data.len() * size_of::<u8>()) as i64);
    }

    /// 读取缓冲区
    pub fn read(
        &mut self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
    ) -> Result<usize, SystemError> {
        let start = self.data.len().min(offset);
        let end = self.data.len().min(offset + len);

        // buffer空间不足
        if buf.len() < (end - start) {
            return Err(SystemError::ENOBUFS);
        }

        // 拷贝数据
        let src = &self.data[start..end];
        buf[0..src.len()].copy_from_slice(src);

        return Ok(src.len());
    }

    /// 将buffer中的LogMessage转成u8数组，方便传入用户的buf
    pub fn tobytes(&mut self) {
        self.data.clear();

        let size = self.buffer.len();
        for _ in 0..size {
            let msg = self.buffer.pop().unwrap();
            self.data.append(
                &mut format!(
                    "({}) [{}] {}\n",
                    msg.time(),
                    LogLevel::level2str(msg.level()),
                    msg.message(),
                )
                .as_bytes()
                .to_owned(),
            );

            // 最后要把弹出的日志消息重新添加回来，否则只要调用一次open之后，缓冲区就空了
            let _ = self.buffer.push(msg);
        }
    }
}
