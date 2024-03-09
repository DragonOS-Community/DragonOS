use alloc::{boxed::Box, sync::Arc, vec::Vec};
use system_error::SystemError;

use crate::{libs::spinlock::SpinLock, net::Endpoint};

use super::{Socket, SocketMetadata, SocketOptions, SocketPair, SocketType, SocketpairOps};

#[derive(Debug, Clone)]
pub struct StreamSocket {
    metadata: SocketMetadata,
    buffer: Arc<SpinLock<Vec<u8>>>,
    peer_buffer: Option<Arc<SpinLock<Vec<u8>>>>,
}

impl StreamSocket {
    /// 默认的元数据缓冲区大小
    pub const DEFAULT_METADATA_BUF_SIZE: usize = 1024;
    /// 默认的缓冲区大小
    pub const DEFAULT_BUF_SIZE: usize = 64 * 1024;

    /// # 创建一个seqpacket的socket
    ///
    /// ## 参数
    /// - `options`: socket的选项
    pub fn new(options: SocketOptions) -> Self {
        let buffer = Vec::with_capacity(Self::DEFAULT_BUF_SIZE);

        let metadata = SocketMetadata::new(
            SocketType::Unix,
            Self::DEFAULT_BUF_SIZE,
            0,
            Self::DEFAULT_METADATA_BUF_SIZE,
            options,
        );

        Self {
            metadata,
            buffer: Arc::new(SpinLock::new(buffer)),
            peer_buffer: None,
        }
    }
}

impl Socket for StreamSocket {
    fn read(&mut self, buf: &mut [u8]) -> (Result<usize, SystemError>, Endpoint) {
        let buffer = self.buffer.lock_irqsave();

        let len = core::cmp::min(buf.len(), buffer.len());
        buf[..len].copy_from_slice(&buffer[..len]);

        (Ok(len), Endpoint::File(None))
    }

    fn write(&self, buf: &[u8], _to: Option<Endpoint>) -> Result<usize, SystemError> {
        if self.peer_buffer.is_none() {
            kwarn!("StreamSocket is now just for socketpair");
            return Err(SystemError::ENOSYS);
        }

        let binding = self.peer_buffer.clone().unwrap();
        let mut peer_buffer = binding.lock_irqsave();

        let len = buf.len();
        if peer_buffer.capacity() - peer_buffer.len() < len {
            return Err(SystemError::ENOBUFS);
        }
        peer_buffer[..len].copy_from_slice(buf);

        Ok(len)
    }

    fn metadata(&self) -> Result<SocketMetadata, SystemError> {
        Ok(self.metadata.clone())
    }

    fn box_clone(&self) -> Box<dyn Socket> {
        Box::new(self.clone())
    }
}

impl SocketPair for StreamSocket {
    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
    }

    fn socketpair_ops(&self) -> Option<&'static dyn SocketpairOps> {
        Some(&SeqpacketSocketpairOps)
    }

    fn buffer(&self) -> Arc<SpinLock<Vec<u8>>> {
        self.buffer.clone()
    }

    fn set_peer_buffer(&mut self, peer_buffer: Arc<SpinLock<Vec<u8>>>) {
        self.peer_buffer = Some(peer_buffer);
    }
}

#[derive(Debug, Clone)]
pub struct SeqpacketSocket {
    metadata: SocketMetadata,
    buffer: Arc<SpinLock<Vec<u8>>>,
    peer_buffer: Option<Arc<SpinLock<Vec<u8>>>>,
}

impl SeqpacketSocket {
    /// 默认的元数据缓冲区大小
    pub const DEFAULT_METADATA_BUF_SIZE: usize = 1024;
    /// 默认的缓冲区大小
    pub const DEFAULT_BUF_SIZE: usize = 64 * 1024;

    /// # 创建一个seqpacket的socket
    ///
    /// ## 参数
    /// - `options`: socket的选项
    pub fn new(options: SocketOptions) -> Self {
        let buffer = Vec::with_capacity(Self::DEFAULT_BUF_SIZE);

        let metadata = SocketMetadata::new(
            SocketType::Unix,
            Self::DEFAULT_BUF_SIZE,
            0,
            Self::DEFAULT_METADATA_BUF_SIZE,
            options,
        );

        Self {
            metadata,
            buffer: Arc::new(SpinLock::new(buffer)),
            peer_buffer: None,
        }
    }
}

impl Socket for SeqpacketSocket {
    fn read(&mut self, buf: &mut [u8]) -> (Result<usize, SystemError>, Endpoint) {
        let buffer = self.buffer.lock_irqsave();

        let len = core::cmp::min(buf.len(), buffer.len());
        buf[..len].copy_from_slice(&buffer[..len]);

        (Ok(len), Endpoint::File(None))
    }

    fn write(&self, buf: &[u8], _to: Option<Endpoint>) -> Result<usize, SystemError> {
        if self.peer_buffer.is_none() {
            kwarn!("SeqpacketSocket is now just for socketpair");
            return Err(SystemError::ENOSYS);
        }

        let binding = self.peer_buffer.clone().unwrap();
        let mut peer_buffer = binding.lock_irqsave();

        let len = buf.len();
        if peer_buffer.capacity() - peer_buffer.len() < len {
            return Err(SystemError::ENOBUFS);
        }
        peer_buffer[..len].copy_from_slice(buf);

        Ok(len)
    }

    fn metadata(&self) -> Result<SocketMetadata, SystemError> {
        Ok(self.metadata.clone())
    }

    fn box_clone(&self) -> Box<dyn Socket> {
        Box::new(self.clone())
    }
}

impl SocketPair for SeqpacketSocket {
    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
    }

    fn socketpair_ops(&self) -> Option<&'static dyn SocketpairOps> {
        Some(&SeqpacketSocketpairOps)
    }

    fn buffer(&self) -> Arc<SpinLock<Vec<u8>>> {
        self.buffer.clone()
    }

    fn set_peer_buffer(&mut self, peer_buffer: Arc<SpinLock<Vec<u8>>>) {
        self.peer_buffer = Some(peer_buffer);
    }
}

struct SeqpacketSocketpairOps;

impl SocketpairOps for SeqpacketSocketpairOps {
    fn socketpair(&self, socket0: &mut Box<dyn SocketPair>, socket1: &mut Box<dyn SocketPair>) {
        let pair0 = socket0
            .as_mut()
            .as_any_mut()
            .downcast_mut::<SeqpacketSocket>()
            .unwrap();

        let pair1 = socket1
            .as_mut()
            .as_any_mut()
            .downcast_mut::<SeqpacketSocket>()
            .unwrap();
        pair0.set_peer_buffer(pair1.buffer());
        pair1.set_peer_buffer(pair0.buffer());
    }
}
