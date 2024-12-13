use alloc::string::String;
use alloc::{collections::VecDeque, sync::Arc};
use core::sync::atomic::{AtomicUsize, Ordering};

use super::SeqpacketSocket;
use crate::{
    libs::mutex::Mutex,
    net::socket::{buffer::Buffer, endpoint::Endpoint, Inode, ShutdownTemp},
};
use system_error::SystemError::{self, *};

#[derive(Debug)]
pub(super) struct Init {
    inode: Option<Endpoint>,
}

impl Init {
    pub(super) fn new() -> Self {
        Self { inode: None }
    }

    pub(super) fn bind(&mut self, epoint_to_bind: Endpoint) -> Result<(), SystemError> {
        if self.inode.is_some() {
            log::error!("the socket is already bound");
            return Err(EINVAL);
        }
        match epoint_to_bind {
            Endpoint::Inode(_) => self.inode = Some(epoint_to_bind),
            _ => return Err(EINVAL),
        }

        return Ok(());
    }

    pub fn bind_path(&mut self, sun_path: String) -> Result<Endpoint, SystemError> {
        if self.inode.is_none() {
            log::error!("the socket is not bound");
            return Err(EINVAL);
        }
        if let Some(Endpoint::Inode((inode, mut path))) = self.inode.take() {
            path = sun_path;
            let epoint = Endpoint::Inode((inode, path));
            self.inode.replace(epoint.clone());
            return Ok(epoint);
        };

        return Err(SystemError::EINVAL);
    }

    pub fn endpoint(&self) -> Option<&Endpoint> {
        return self.inode.as_ref();
    }
}

#[derive(Debug)]
pub(super) struct Listener {
    inode: Endpoint,
    backlog: AtomicUsize,
    incoming_conns: Mutex<VecDeque<Arc<Inode>>>,
}

impl Listener {
    pub(super) fn new(inode: Endpoint, backlog: usize) -> Self {
        log::debug!("backlog {}", backlog);
        let back = if backlog > 1024 { 1024_usize } else { backlog };
        return Self {
            inode,
            backlog: AtomicUsize::new(back),
            incoming_conns: Mutex::new(VecDeque::with_capacity(back)),
        };
    }
    pub(super) fn endpoint(&self) -> &Endpoint {
        return &self.inode;
    }

    pub(super) fn try_accept(&self) -> Result<(Arc<Inode>, Endpoint), SystemError> {
        let mut incoming_conns = self.incoming_conns.lock();
        log::debug!(" incom len {}", incoming_conns.len());
        let conn = incoming_conns
            .pop_front()
            .ok_or(SystemError::EAGAIN_OR_EWOULDBLOCK)?;
        let socket =
            Arc::downcast::<SeqpacketSocket>(conn.inner()).map_err(|_| SystemError::EINVAL)?;
        let peer = match &*socket.inner.read() {
            Inner::Connected(connected) => connected.peer_endpoint().unwrap().clone(),
            _ => return Err(SystemError::ENOTCONN),
        };

        return Ok((Inode::new(socket), peer));
    }

    pub(super) fn listen(&self, backlog: usize) -> Result<(), SystemError> {
        self.backlog.store(backlog, Ordering::Relaxed);
        Ok(())
    }

    pub(super) fn push_incoming(
        &self,
        client_epoint: Option<Endpoint>,
    ) -> Result<Connected, SystemError> {
        let mut incoming_conns = self.incoming_conns.lock();
        if incoming_conns.len() >= self.backlog.load(Ordering::Relaxed) {
            log::error!("the pending connection queue on the listening socket is full");
            return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
        }

        let new_server = SeqpacketSocket::new(false);
        let new_inode = Inode::new(new_server.clone());
        // log::debug!("new inode {:?},client_epoint {:?}",new_inode,client_epoint);
        let path = match &self.inode {
            Endpoint::Inode((_, path)) => path.clone(),
            _ => return Err(SystemError::EINVAL),
        };

        let (server_conn, client_conn) = Connected::new_pair(
            Some(Endpoint::Inode((new_inode.clone(), path))),
            client_epoint,
        );
        *new_server.inner.write() = Inner::Connected(server_conn);
        incoming_conns.push_back(new_inode);

        // TODO: epollin

        Ok(client_conn)
    }

    pub(super) fn is_acceptable(&self) -> bool {
        return self.incoming_conns.lock().len() != 0;
    }
}

#[derive(Debug)]
pub struct Connected {
    inode: Option<Endpoint>,
    peer_inode: Option<Endpoint>,
    buffer: Arc<Buffer>,
}

impl Connected {
    /// 默认的缓冲区大小
    pub const DEFAULT_BUF_SIZE: usize = 64 * 1024;

    pub fn new_pair(
        inode: Option<Endpoint>,
        peer_inode: Option<Endpoint>,
    ) -> (Connected, Connected) {
        let this = Connected {
            inode: inode.clone(),
            peer_inode: peer_inode.clone(),
            buffer: Buffer::new(),
        };
        let peer = Connected {
            inode: peer_inode,
            peer_inode: inode,
            buffer: Buffer::new(),
        };

        (this, peer)
    }

    pub fn set_peer_inode(&mut self, peer_epoint: Option<Endpoint>) {
        self.peer_inode = peer_epoint;
    }

    pub fn set_inode(&mut self, epoint: Option<Endpoint>) {
        self.inode = epoint;
    }

    pub fn endpoint(&self) -> Option<&Endpoint> {
        self.inode.as_ref()
    }

    pub fn peer_endpoint(&self) -> Option<&Endpoint> {
        self.peer_inode.as_ref()
    }

    pub fn try_read(&self, buf: &mut [u8]) -> Result<usize, SystemError> {
        if self.can_recv() {
            return self.recv_slice(buf);
        } else {
            return Err(SystemError::EINVAL);
        }
    }

    pub fn try_write(&self, buf: &[u8]) -> Result<usize, SystemError> {
        if self.can_send()? {
            return self.send_slice(buf);
        } else {
            log::debug!("can not send {:?}", String::from_utf8_lossy(buf));
            return Err(SystemError::ENOBUFS);
        }
    }

    pub fn can_recv(&self) -> bool {
        return !self.buffer.is_read_buf_empty();
    }

    // 检查发送缓冲区是否满了
    pub fn can_send(&self) -> Result<bool, SystemError> {
        // let sebuffer = self.sebuffer.lock(); // 获取锁
        // sebuffer.capacity()-sebuffer.len() ==0;
        let peer_inode = match self.peer_inode.as_ref().unwrap() {
            Endpoint::Inode((inode, _)) => inode,
            _ => return Err(SystemError::EINVAL),
        };
        let peer_socket = Arc::downcast::<SeqpacketSocket>(peer_inode.inner())
            .map_err(|_| SystemError::EINVAL)?;
        let is_full = match &*peer_socket.inner.read() {
            Inner::Connected(connected) => connected.buffer.is_read_buf_full(),
            _ => return Err(SystemError::EINVAL),
        };
        Ok(!is_full)
    }

    pub fn recv_slice(&self, buf: &mut [u8]) -> Result<usize, SystemError> {
        return self.buffer.read_read_buffer(buf);
    }

    pub fn send_slice(&self, buf: &[u8]) -> Result<usize, SystemError> {
        //找到peer_inode，并将write_buffer的内容写入对端的read_buffer
        let peer_inode = match self.peer_inode.as_ref().unwrap() {
            Endpoint::Inode((inode, _)) => inode,
            _ => return Err(SystemError::EINVAL),
        };
        let peer_socket = Arc::downcast::<SeqpacketSocket>(peer_inode.inner())
            .map_err(|_| SystemError::EINVAL)?;
        let usize = match &*peer_socket.inner.write() {
            Inner::Connected(connected) => {
                let usize = connected.buffer.write_read_buffer(buf)?;
                usize
            }
            _ => return Err(SystemError::EINVAL),
        };
        peer_socket.wait_queue.wakeup(None);
        Ok(usize)
    }

    pub fn shutdown(&self, how: ShutdownTemp) -> Result<(), SystemError> {
        if how.is_empty() {
            return Err(SystemError::EINVAL);
        } else if how.is_send_shutdown() {
            unimplemented!("unimplemented!");
        } else if how.is_recv_shutdown() {
            unimplemented!("unimplemented!");
        }

        Ok(())
    }
}

#[derive(Debug)]
pub(super) enum Inner {
    Init(Init),
    Listen(Listener),
    Connected(Connected),
}
