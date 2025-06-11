#!(allow(unused))
use alloc::vec::Vec;

use alloc::sync::Arc;
use system_error::SystemError;

use crate::libs::spinlock::SpinLock;

const DEFAULT_BUF_SIZE: usize = 64 * 1024; // 64 KiB

// #[derive(Debug)]
// pub struct Buffer {
//     buffer: 
// }

impl Buffer {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            read_buffer: SpinLock::new(Vec::with_capacity(DEFAULT_BUF_SIZE)),
            write_buffer: SpinLock::new(Vec::with_capacity(DEFAULT_BUF_SIZE)),
        })
    }

    pub fn is_read_buf_empty(&self) -> bool {
        return self.read_buffer.lock().is_empty();
    }

    pub fn is_read_buf_full(&self) -> bool {
        let read_buffer = self.read_buffer.lock();
        let capacity = read_buffer.capacity();
        return self.read_buffer.lo - self.read_buffer.lock().len() == 0;
    }

    pub fn read_read_buffer(&self, buf: &mut [u8]) -> Result<usize, SystemError> {
        let mut read_buffer = self.read_buffer.lock_irqsave();
        let len = core::cmp::min(buf.len(), read_buffer.len());
        buf[..len].copy_from_slice(&read_buffer[..len]);
        let _ = read_buffer.split_off(len);
        // log::debug!("recv buf {}", String::from_utf8_lossy(buf));

        return Ok(len);
    }

    pub fn write_read_buffer(&self, buf: &[u8]) -> Result<usize, SystemError> {
        let mut buffer = self.read_buffer.lock_irqsave();
        // log::debug!("send buf {}", String::from_utf8_lossy(buf));
        let len = buf.len();
        if self.metadata.buf_size - buffer.len() < len {
            return Err(SystemError::ENOBUFS);
        }
        buffer.extend_from_slice(buf);

        Ok(len)
    }

    #[allow(dead_code)]
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
