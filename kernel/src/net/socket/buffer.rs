use alloc::vec::Vec;

use alloc::{string::String, sync::Arc};
use log::debug;
use system_error::SystemError;

use crate::libs::spinlock::SpinLock;

#[derive(Debug)]
pub struct Buffer {
    metadata: Metadata,
    read_buffer: SpinLock<Vec<u8>>,
    write_buffer: SpinLock<Vec<u8>>,
}

impl Buffer {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            metadata: Metadata::default(),
            read_buffer: SpinLock::new(Vec::new()),
            write_buffer: SpinLock::new(Vec::new()),
        })
    }

    pub fn is_read_buf_empty(&self) -> bool {
        return self.read_buffer.lock().is_empty();
    }

    pub fn is_read_buf_full(&self) -> bool {
        return self.metadata.buf_size - self.read_buffer.lock().len() == 0;
    }

    pub fn is_write_buf_empty(&self) -> bool {
        return self.write_buffer.lock().is_empty();
    }

    pub fn is_write_buf_full(&self) -> bool {
        return self.write_buffer.lock().len() >= self.metadata.buf_size;
    }

    pub fn read_read_buffer(&self, buf: &mut [u8]) -> Result<usize, SystemError> {
        let mut read_buffer = self.read_buffer.lock_irqsave();
        let len = core::cmp::min(buf.len(), read_buffer.len());
        buf[..len].copy_from_slice(&read_buffer[..len]);
        let _ = read_buffer.split_off(len);
        log::debug!("recv buf {}", String::from_utf8_lossy(buf));

        return Ok(len);
    }

    pub fn write_read_buffer(&self, buf: &[u8]) -> Result<usize, SystemError> {
        let mut buffer = self.read_buffer.lock_irqsave();
        log::debug!("send buf {}", String::from_utf8_lossy(buf));
        let len = buf.len();
        if self.metadata.buf_size - buffer.len() < len {
            return Err(SystemError::ENOBUFS);
        }
        buffer.extend_from_slice(buf);

        Ok(len)
    }

    pub fn write_write_buffer(&self, buf: &[u8]) -> Result<usize, SystemError> {
        let mut buffer = self.write_buffer.lock_irqsave();

        let len = buf.len();
        if self.metadata.buf_size - buffer.len() < len {
            return Err(SystemError::ENOBUFS);
        }
        buffer.extend_from_slice(buf);

        Ok(len)
    }
}

#[derive(Debug)]
pub struct Metadata {
    /// 默认的元数据缓冲区大小
    metadata_buf_size: usize,
    /// 默认的缓冲区大小
    buf_size: usize,
}

impl Default for Metadata {
    fn default() -> Self {
        Self {
            metadata_buf_size: 1024,
            buf_size: 64 * 1024,
        }
    }
}
