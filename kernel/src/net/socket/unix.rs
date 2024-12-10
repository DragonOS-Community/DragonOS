use alloc::{boxed::Box, sync::Arc, vec::Vec};
use system_error::SystemError;

use crate::{libs::spinlock::SpinLock, net::Endpoint};

use super::{
    handle::GlobalSocketHandle, PosixSocketHandleItem, Socket, SocketInode, SocketMetadata,
    SocketOptions, SocketType,
};

#[derive(Debug, Clone)]
pub struct StreamSocket {
    metadata: SocketMetadata,
    buffer: Arc<SpinLock<Vec<u8>>>,
    peer_inode: Option<Arc<SocketInode>>,
    handle: GlobalSocketHandle,
    posix_item: Arc<PosixSocketHandleItem>,
}

impl StreamSocket {
    /// 默认的元数据缓冲区大小
    pub const DEFAULT_METADATA_BUF_SIZE: usize = 1024;
    /// 默认的缓冲区大小
    pub const DEFAULT_BUF_SIZE: usize = 64 * 1024;

    /// # 创建一个 Stream Socket
    ///
    /// ## 参数
    /// - `options`: socket选项
    pub fn new(options: SocketOptions) -> Self {
        let buffer = Arc::new(SpinLock::new(Vec::with_capacity(Self::DEFAULT_BUF_SIZE)));

        let metadata = SocketMetadata::new(
            SocketType::Unix,
            Self::DEFAULT_BUF_SIZE,
            Self::DEFAULT_BUF_SIZE,
            Self::DEFAULT_METADATA_BUF_SIZE,
            options,
        );

        let posix_item = Arc::new(PosixSocketHandleItem::new(None));

        Self {
            metadata,
            buffer,
            peer_inode: None,
            handle: GlobalSocketHandle::new_kernel_handle(),
            posix_item,
        }
    }
}

impl Socket for StreamSocket {
    fn posix_item(&self) -> Arc<PosixSocketHandleItem> {
        self.posix_item.clone()
    }
    fn socket_handle(&self) -> GlobalSocketHandle {
        self.handle
    }

    fn close(&mut self) {}

    fn read(&self, buf: &mut [u8]) -> (Result<usize, SystemError>, Endpoint) {
        let mut buffer = self.buffer.lock_irqsave();

        let len = core::cmp::min(buf.len(), buffer.len());
        buf[..len].copy_from_slice(&buffer[..len]);

        let _ = buffer.split_off(len);

        (Ok(len), Endpoint::Inode(self.peer_inode.clone()))
    }

    fn write(&self, buf: &[u8], _to: Option<Endpoint>) -> Result<usize, SystemError> {
        if self.peer_inode.is_none() {
            return Err(SystemError::ENOTCONN);
        }

        let peer_inode = self.peer_inode.clone().unwrap();
        let len = peer_inode.inner().write_buffer(buf)?;
        Ok(len)
    }

    fn connect(&mut self, endpoint: Endpoint) -> Result<(), SystemError> {
        if self.peer_inode.is_some() {
            return Err(SystemError::EISCONN);
        }

        if let Endpoint::Inode(inode) = endpoint {
            self.peer_inode = inode;
            Ok(())
        } else {
            Err(SystemError::EINVAL)
        }
    }

    fn write_buffer(&self, buf: &[u8]) -> Result<usize, SystemError> {
        let mut buffer = self.buffer.lock_irqsave();

        let len = buf.len();
        if buffer.capacity() - buffer.len() < len {
            return Err(SystemError::ENOBUFS);
        }
        buffer.extend_from_slice(buf);

        Ok(len)
    }

    fn metadata(&self) -> SocketMetadata {
        self.metadata.clone()
    }

    fn box_clone(&self) -> Box<dyn Socket> {
        Box::new(self.clone())
    }

    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
    }
}

#[derive(Debug, Clone)]
pub struct SeqpacketSocket {
    metadata: SocketMetadata,
    buffer: Arc<SpinLock<Vec<u8>>>,
    peer_inode: Option<Arc<SocketInode>>,
    handle: GlobalSocketHandle,
    posix_item: Arc<PosixSocketHandleItem>,
}

impl SeqpacketSocket {
    /// 默认的元数据缓冲区大小
    pub const DEFAULT_METADATA_BUF_SIZE: usize = 1024;
    /// 默认的缓冲区大小
    pub const DEFAULT_BUF_SIZE: usize = 64 * 1024;

    /// # 创建一个 Seqpacket Socket
    ///
    /// ## 参数
    /// - `options`: socket选项
    pub fn new(options: SocketOptions) -> Self {
        let buffer = Arc::new(SpinLock::new(Vec::with_capacity(Self::DEFAULT_BUF_SIZE)));

        let metadata = SocketMetadata::new(
            SocketType::Unix,
            Self::DEFAULT_BUF_SIZE,
            Self::DEFAULT_BUF_SIZE,
            Self::DEFAULT_METADATA_BUF_SIZE,
            options,
        );

        let posix_item = Arc::new(PosixSocketHandleItem::new(None));

        Self {
            metadata,
            buffer,
            peer_inode: None,
            handle: GlobalSocketHandle::new_kernel_handle(),
            posix_item,
        }
    }
}

impl Socket for SeqpacketSocket {
    fn posix_item(&self) -> Arc<PosixSocketHandleItem> {
        self.posix_item.clone()
    }
    fn close(&mut self) {}

    fn read(&self, buf: &mut [u8]) -> (Result<usize, SystemError>, Endpoint) {
        let mut buffer = self.buffer.lock_irqsave();

        let len = core::cmp::min(buf.len(), buffer.len());
        buf[..len].copy_from_slice(&buffer[..len]);

        let _ = buffer.split_off(len);

        (Ok(len), Endpoint::Inode(self.peer_inode.clone()))
    }

    fn write(&self, buf: &[u8], _to: Option<Endpoint>) -> Result<usize, SystemError> {
        if self.peer_inode.is_none() {
            return Err(SystemError::ENOTCONN);
        }

        let peer_inode = self.peer_inode.clone().unwrap();
        let len = peer_inode.inner().write_buffer(buf)?;
        Ok(len)
    }

    fn connect(&mut self, endpoint: Endpoint) -> Result<(), SystemError> {
        if self.peer_inode.is_some() {
            return Err(SystemError::EISCONN);
        }

        if let Endpoint::Inode(inode) = endpoint {
            self.peer_inode = inode;
            Ok(())
        } else {
            Err(SystemError::EINVAL)
        }
    }

    fn write_buffer(&self, buf: &[u8]) -> Result<usize, SystemError> {
        let mut buffer = self.buffer.lock_irqsave();

        let len = buf.len();
        if buffer.capacity() - buffer.len() < len {
            return Err(SystemError::ENOBUFS);
        }
        buffer.extend_from_slice(buf);

        Ok(len)
    }

    fn socket_handle(&self) -> GlobalSocketHandle {
        self.handle
    }

    fn metadata(&self) -> SocketMetadata {
        self.metadata.clone()
    }

    fn box_clone(&self) -> Box<dyn Socket> {
        Box::new(self.clone())
    }

    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
    }
}
