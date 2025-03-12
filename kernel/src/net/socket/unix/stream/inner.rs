use core::sync::atomic::{AtomicUsize, Ordering};

use log::debug;
use system_error::SystemError;

use crate::libs::mutex::Mutex;
use crate::net::socket::buffer::Buffer;
use crate::net::socket::unix::stream::StreamSocket;
use crate::net::socket::{Endpoint, Inode, ShutdownTemp};

use alloc::collections::VecDeque;
use alloc::{string::String, sync::Arc};

#[derive(Debug)]
pub enum Inner {
    Init(Init),
    Connected(Connected),
    Listener(Listener),
}

#[derive(Debug)]
pub struct Init {
    addr: Option<Endpoint>,
}

impl Init {
    pub(super) fn new() -> Self {
        Self { addr: None }
    }

    pub(super) fn bind(&mut self, endpoint_to_bind: Endpoint) -> Result<(), SystemError> {
        if self.addr.is_some() {
            log::error!("the socket is already bound");
            return Err(SystemError::EINVAL);
        }

        match endpoint_to_bind {
            Endpoint::Inode(_) => self.addr = Some(endpoint_to_bind),
            _ => return Err(SystemError::EINVAL),
        }

        return Ok(());
    }

    pub fn bind_path(&mut self, sun_path: String) -> Result<Endpoint, SystemError> {
        if self.addr.is_none() {
            log::error!("the socket is not bound");
            return Err(SystemError::EINVAL);
        }
        if let Some(Endpoint::Inode((inode, mut path))) = self.addr.take() {
            path = sun_path;
            let epoint = Endpoint::Inode((inode, path.clone()));
            self.addr.replace(epoint.clone());
            log::debug!("bind path in inode : {:?}", path);
            return Ok(epoint);
        };

        return Err(SystemError::EINVAL);
    }

    pub(super) fn endpoint(&self) -> Option<&Endpoint> {
        self.addr.as_ref()
    }
}

#[derive(Debug, Clone)]
pub struct Connected {
    addr: Option<Endpoint>,
    peer_addr: Option<Endpoint>,
    buffer: Arc<Buffer>,
}

impl Connected {
    pub fn new_pair(addr: Option<Endpoint>, peer_addr: Option<Endpoint>) -> (Self, Self) {
        let this = Connected {
            addr: addr.clone(),
            peer_addr: peer_addr.clone(),
            buffer: Buffer::new(),
        };
        let peer = Connected {
            addr: peer_addr,
            peer_addr: addr,
            buffer: Buffer::new(),
        };

        return (this, peer);
    }

    pub fn endpoint(&self) -> Option<&Endpoint> {
        self.addr.as_ref()
    }

    pub fn set_addr(&mut self, addr: Option<Endpoint>) {
        self.addr = addr;
    }

    pub fn peer_endpoint(&self) -> Option<&Endpoint> {
        self.peer_addr.as_ref()
    }

    pub fn set_peer_addr(&mut self, peer: Option<Endpoint>) {
        self.peer_addr = peer;
    }

    pub fn send_slice(&self, buf: &[u8]) -> Result<usize, SystemError> {
        //写入对端buffer
        let peer_inode = match self.peer_addr.as_ref().unwrap() {
            Endpoint::Inode((inode, _)) => inode,
            _ => return Err(SystemError::EINVAL),
        };
        let peer_socket =
            Arc::downcast::<StreamSocket>(peer_inode.inner()).map_err(|_| SystemError::EINVAL)?;
        let usize = match &*peer_socket.inner.read() {
            Inner::Connected(conntected) => {
                let usize = conntected.buffer.write_read_buffer(buf)?;
                usize
            }
            _ => {
                debug!("no! is not connested!");
                return Err(SystemError::EINVAL);
            }
        };
        peer_socket.wait_queue.wakeup(None);
        Ok(usize)
    }

    pub fn can_send(&self) -> Result<bool, SystemError> {
        //查看连接体里的buf是否非满
        let peer_inode = match self.peer_addr.as_ref().unwrap() {
            Endpoint::Inode((inode, _)) => inode,
            _ => return Err(SystemError::EINVAL),
        };
        let peer_socket =
            Arc::downcast::<StreamSocket>(peer_inode.inner()).map_err(|_| SystemError::EINVAL)?;
        let is_full = match &*peer_socket.inner.read() {
            Inner::Connected(connected) => connected.buffer.is_read_buf_full(),
            _ => return Err(SystemError::EINVAL),
        };
        debug!("can send? :{}", !is_full);
        Ok(!is_full)
    }

    pub fn can_recv(&self) -> bool {
        //查看连接体里的buf是否非空
        return !self.buffer.is_read_buf_empty();
    }

    pub fn try_send(&self, buf: &[u8]) -> Result<usize, SystemError> {
        if self.can_send()? {
            return self.send_slice(buf);
        } else {
            return Err(SystemError::ENOBUFS);
        }
    }

    fn recv_slice(&self, buf: &mut [u8]) -> Result<usize, SystemError> {
        return self.buffer.read_read_buffer(buf);
    }

    pub fn try_recv(&self, buf: &mut [u8]) -> Result<usize, SystemError> {
        if self.can_recv() {
            return self.recv_slice(buf);
        } else {
            return Err(SystemError::EINVAL);
        }
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
pub struct Listener {
    addr: Option<Endpoint>,
    incoming_connects: Mutex<VecDeque<Arc<Inode>>>,
    backlog: AtomicUsize,
}

impl Listener {
    pub fn new(addr: Option<Endpoint>, backlog: usize) -> Self {
        Self {
            addr,
            incoming_connects: Mutex::new(VecDeque::new()),
            backlog: AtomicUsize::new(backlog),
        }
    }

    pub fn listen(&self, backlog: usize) -> Result<(), SystemError> {
        self.backlog.store(backlog, Ordering::Relaxed);
        return Ok(());
    }

    pub fn push_incoming(&self, server_inode: Arc<Inode>) -> Result<(), SystemError> {
        let mut incoming_connects = self.incoming_connects.lock();

        if incoming_connects.len() >= self.backlog.load(Ordering::Relaxed) {
            debug!("unix stream listen socket connected queue is full!");
            return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
        }

        incoming_connects.push_back(server_inode);

        return Ok(());
    }

    pub fn pop_incoming(&self) -> Option<Arc<Inode>> {
        let mut incoming_connects = self.incoming_connects.lock();

        return incoming_connects.pop_front();
    }

    pub(super) fn endpoint(&self) -> Option<&Endpoint> {
        self.addr.as_ref()
    }

    pub(super) fn is_acceptable(&self) -> bool {
        return self.incoming_connects.lock().len() != 0;
    }

    pub(super) fn try_accept(&self) -> Result<(Arc<Inode>, Endpoint), SystemError> {
        let mut incoming_connecteds = self.incoming_connects.lock();
        debug!("incom len {}", incoming_connecteds.len());
        let connected = incoming_connecteds
            .pop_front()
            .ok_or(SystemError::EAGAIN_OR_EWOULDBLOCK)?;
        let socket =
            Arc::downcast::<StreamSocket>(connected.inner()).map_err(|_| SystemError::EINVAL)?;
        let peer = match &*socket.inner.read() {
            Inner::Connected(connected) => connected.peer_endpoint().unwrap().clone(),
            _ => return Err(SystemError::ENOTCONN),
        };
        debug!("server accept!");
        return Ok((Inode::new(socket), peer));
    }
}
