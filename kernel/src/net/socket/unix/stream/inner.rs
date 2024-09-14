use core::sync::atomic::{AtomicUsize, Ordering};

use log::debug;
use system_error::SystemError;

use crate::libs::mutex::Mutex;
use crate::net::socket::buffer::Buffer;
use crate::net::socket::unix::PNODE_TABLE;
use crate::net::socket::{Endpoint, Inode, ShutdownTemp};

use alloc::collections::VecDeque;
use alloc::sync::Arc;

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
        Self { addr: None}
    }

    pub(super) fn bind(&mut self, endpoint_to_bind: Endpoint, snode: Arc<Inode>) -> Result<(), SystemError> {
        self.addr = Some(endpoint_to_bind.clone());
        // 获得pnode的inode id
        let inode_id = match endpoint_to_bind {
            Endpoint::Pnode(pnode) => {
                pnode.metadata().unwrap().inode_id.data()
            },
            _ => return Err(SystemError::EINVAL),
        };

        PNODE_TABLE.add_entry(&inode_id, snode)
    }

    pub(super) fn addr(&self) -> Option<Endpoint> {
        self.addr.clone()
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
        let this = Connected{
            addr: addr.clone(), 
            peer_addr: peer_addr.clone(), 
            buffer: Buffer::new()
        };
        let peer = Connected{
            addr: addr.clone(), 
            peer_addr: peer_addr.clone(), 
            buffer: Buffer::new()
        };

        return (this, peer);
    }

    pub fn addr(&self) -> Option<&Endpoint> {
        self.addr.as_ref()
    }

    pub fn set_addr(&mut self, addr: Option<Endpoint>) {
        self.addr = addr;
    }

    pub fn peer_addr(&self) -> Option<&Endpoint> {
        self.peer_addr.as_ref()
    }

    pub fn set_peer_addr(&mut self, peer: Option<Endpoint>) {
        self.peer_addr = peer;
    }

    fn send_slice(&self, buf: &[u8]) -> Result<usize, SystemError> {
        //写入buffer
        return self.buffer.write_write_buffer(buf);
    }

    fn can_send(&self) -> bool{
        //查看连接体里的buf是否非满
        return self.buffer.is_write_buf_full();
    }

    fn can_recv(&self) -> bool {
        //查看连接体里的buf是否非空
        return !self.buffer.is_read_buf_empty();
    }

    pub fn try_send(&self, buf: &[u8]) -> Result<usize, SystemError> {
        if self.can_send() {
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

    pub(super) fn addr(&self) -> Option<Endpoint> {
        self.addr.clone()
    }
}
