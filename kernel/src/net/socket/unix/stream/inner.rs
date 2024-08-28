use core::sync::atomic::{AtomicUsize, Ordering};

use log::debug;
use system_error::SystemError;

use crate::libs::mutex::Mutex;
use crate::net::Endpoint;

use alloc::collections::VecDeque;


#[derive(Debug, Clone)]
pub enum Inner {
    Init(Init),
    Connected(Connected),
    Listener(Listener),
}

#[derive(Debug, Clone)]
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
}

#[derive(Debug, Clone)]
pub struct Listener {
    addr: Option<Endpoint>,
}

impl Listener {
    pub fn new(addr: Option<Endpoint>, backlog: usize) -> Self {}
}

pub struct Backlog {
    addr: Option<Endpoint>,
    incoming_connects: Mutex<VecDeque<Connected>>,
    backlog: AtomicUsize,
}

impl Backlog {
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
}

// static BACKLOG_TABLE: BacklogTable = BacklogTable::new();

// struct BacklogTable {
//     backlog_sockets: RwLock<BTreeMap<Option<Endpoint>, Arc<Backlog>>>,
// }

// impl BacklogTable {
//     const fn new() -> Self {
//         Self {
//             backlog_sockets: RwLock::new(BTreeMap::new()),
//         }
//     }

//     fn add_backlog(&self, addr: Option<Endpoint>, backlog: usize) -> Result<(), SystemError>{
//         let mut backlog_sockets = self.backlog_sockets.write();
//         if backlog_sockets.contains_key(&addr) {
//             return Err(SystemError::EADDRINUSE);
//         }
//         let 
//         Ok(())
//     }
// }
