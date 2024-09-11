use core::sync::atomic::{AtomicUsize, Ordering};

use log::debug;
use system_error::SystemError;

use crate::libs::mutex::Mutex;
use crate::net::socket::{Endpoint, ShutdownTemp};

use alloc::collections::VecDeque;

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
        self.addr = Some(endpoint_to_bind);
        Ok(())
    }

    pub(super) fn addr(&self) -> Option<Endpoint> {
        self.addr.clone()
    }
}

#[derive(Debug, Clone)]
pub struct Connected {
    addr: Option<Endpoint>,
    peer_addr: Option<Endpoint>,
}

impl Connected {
    pub fn new(addr: Option<Endpoint>, peer_addr: Option<Endpoint>) -> Self {
        Self { addr, peer_addr }
    }

    pub fn new_pair(addr: Option<Endpoint>, peer_addr: Option<Endpoint>) -> (Connected, Connected) {
        let this = Connected::new(addr.clone(), peer_addr.clone());
        let peer = Connected::new(peer_addr.clone(), addr.clone());

        return (this, peer);
    }

    pub(super) fn addr(&self) -> Option<Endpoint> {
        self.addr.clone()
    }

    pub fn peer_addr(&self) -> Option<Endpoint> {
        self.peer_addr.clone()
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
    incoming_connects: Mutex<VecDeque<Connected>>,
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

    pub fn push_incoming(&self, client_addr: Option<Endpoint>) -> Result<Connected, SystemError> {
        let mut incoming_connects = self.incoming_connects.lock();

        if incoming_connects.len() >= self.backlog.load(Ordering::Relaxed) {
            debug!("unix stream listen socket connected queue is full!");
            return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
        }

        let (server_conn, client_conn) = Connected::new_pair(self.addr.clone(), client_addr);

        incoming_connects.push_back(server_conn);

        return Ok(client_conn);
    }

    pub fn pop_incoming(&self) -> Option<Connected> {
        let mut incoming_connects = self.incoming_connects.lock();

        return incoming_connects.pop_front();
    }

    pub(super) fn addr(&self) -> Option<Endpoint> {
        self.addr.clone()
    }
}
