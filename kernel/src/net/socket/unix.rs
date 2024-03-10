use alloc::{boxed::Box, sync::Arc, vec::Vec};
use system_error::SystemError;

use crate::{libs::spinlock::SpinLock, net::Endpoint};

use super::{Socket, SocketInode, SocketMetadata, SocketOptions, SocketPair, SocketType};

#[derive(Debug, Clone)]
pub struct StreamSocket {
    metadata: SocketMetadata,
    buffer: Arc<SpinLock<Vec<u8>>>,
    peer_inode: Option<Arc<SocketInode>>,
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

        Self {
            metadata,
            buffer,
            peer_inode: None,
        }
    }
}

impl Socket for StreamSocket {
    fn read(&self, buf: &mut [u8]) -> (Result<usize, SystemError>, Endpoint) {
        let buffer = self.buffer.lock_irqsave();

        let len = core::cmp::min(buf.len(), buffer.len());
        buf[..len].copy_from_slice(&buffer[..len]);

        (Ok(len), Endpoint::Inode(self.peer_inode.clone()))
    }

    fn write(&self, buf: &[u8], _to: Option<Endpoint>) -> Result<usize, SystemError> {
        if self.peer_inode.is_none() {
            return Err(SystemError::ENOTCONN);
        }

        let peer_inode = self.peer_inode.clone().unwrap();
        let len = peer_inode.write_buffer(buf)?;
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

impl SocketPair for StreamSocket {
    // fn socketpair_ops(&self) -> Option<&'static dyn SocketpairOps> {
    //     Some(&SeqpacketSocketpairOps)
    // }

    // fn buffer(&self) -> Arc<SpinLock<Vec<u8>>> {
    //     self.buffer.clone()
    // }

    // fn set_peer_buffer(&mut self, peer_buffer: Arc<SpinLock<Vec<u8>>>) {
    //     self.peer_inode = Some(peer_buffer);
    // }

    fn write_buffer(&self, buf: &[u8]) -> Result<usize, SystemError> {
        let mut buffer = self.buffer.lock_irqsave();

        let len = buf.len();
        if buffer.capacity() - buffer.len() < len {
            return Err(SystemError::ENOBUFS);
        }
        buffer[..len].copy_from_slice(buf);

        Ok(len)
    }
}

#[derive(Debug, Clone)]
pub struct SeqpacketSocket {
    metadata: SocketMetadata,
    buffer: Arc<SpinLock<Vec<u8>>>,
    peer_inode: Option<Arc<SocketInode>>,
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

        Self {
            metadata,
            buffer,
            peer_inode: None,
        }
    }
}

impl Socket for SeqpacketSocket {
    fn read(&self, buf: &mut [u8]) -> (Result<usize, SystemError>, Endpoint) {
        let buffer = self.buffer.lock_irqsave();

        let len = core::cmp::min(buf.len(), buffer.len());
        buf[..len].copy_from_slice(&buffer[..len]);

        (Ok(len), Endpoint::Inode(self.peer_inode.clone()))
    }

    fn write(&self, buf: &[u8], _to: Option<Endpoint>) -> Result<usize, SystemError> {
        if self.peer_inode.is_none() {
            return Err(SystemError::ENOTCONN);
        }

        let peer_inode = self.peer_inode.clone().unwrap();
        let len = peer_inode.write_buffer(buf)?;
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

impl SocketPair for SeqpacketSocket {
    // fn socketpair_ops(&self) -> Option<&'static dyn SocketpairOps> {
    //     Some(&SeqpacketSocketpairOps)
    // }

    // fn buffer(&self) -> Arc<SpinLock<Vec<u8>>> {
    //     self.buffer.clone()
    // }

    // fn set_peer_buffer(&mut self, peer_buffer: Arc<SpinLock<Vec<u8>>>) {
    //     self.peer_inode = Some(peer_buffer);
    // }

    fn write_buffer(&self, buf: &[u8]) -> Result<usize, SystemError> {
        let mut buffer = self.buffer.lock_irqsave();

        let len = buf.len();
        if buffer.capacity() - buffer.len() < len {
            return Err(SystemError::ENOBUFS);
        }
        buffer[..len].copy_from_slice(buf);

        Ok(len)
    }
}

// struct SeqpacketSocketpairOps;

// impl SocketpairOps for SeqpacketSocketpairOps {
//     fn socketpair(&self, socket0: &mut Box<dyn SocketPair>, socket1: &mut Box<dyn SocketPair>) {
//         let pair0 = socket0
//             .as_mut()
//             .as_any_mut()
//             .downcast_mut::<SeqpacketSocket>()
//             .unwrap();

//         let pair1 = socket1
//             .as_mut()
//             .as_any_mut()
//             .downcast_mut::<SeqpacketSocket>()
//             .unwrap();
//         pair0.set_peer_buffer(pair1.buffer());
//         pair1.set_peer_buffer(pair0.buffer());
//     }
// }
