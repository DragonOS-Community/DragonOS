use crate::filesystem::epoll::EPollEventType;
use crate::filesystem::vfs::file::File;
use crate::libs::rwlock::RwLock;
use crate::libs::spinlock::SpinLock;
use crate::libs::wait_queue::WaitQueue;
use crate::net::socket::endpoint::Endpoint;
use crate::net::socket::unix::ring_buffer::{RbConsumer, RbProducer, RingBuffer};
use crate::net::socket::unix::stream::UnixStreamSocket;
use crate::net::socket::unix::{UnixEndpoint, UnixEndpointBound};
use crate::net::socket::Socket;
use alloc::collections::BTreeMap;
use alloc::collections::VecDeque;
use alloc::vec::Vec;
use alloc::{string::String, sync::Arc};
use core::sync::atomic::{AtomicUsize, Ordering};
use log::debug;
use system_error::SystemError;

pub(in crate::net) const UNIX_STREAM_DEFAULT_BUF_SIZE: usize = 65536;

#[derive(Debug)]
pub(super) enum Inner {
    Init(Init),
    Connected(Connected),
    Listener(Listener),
}

impl Inner {
    pub(super) fn check_io_events(&self) -> EPollEventType {
        let mut events = EPollEventType::empty();

        events |= match self {
            Inner::Init(init) => init.check_io_events(),
            Inner::Connected(connected) => connected.check_io_events(),
            Inner::Listener(listener) => listener.check_io_events(),
        };

        events
    }
}

#[derive(Debug)]
pub struct Init {
    addr: Option<UnixEndpointBound>,
    // todo shutdown
    // is_read_shutdown: AtomicBool,
    // is_write_shutdown: AtomicBool,
}

impl Init {
    pub(super) fn new() -> Self {
        Self { addr: None }
    }

    pub(super) fn bind(&mut self, endpoint_to_bind: UnixEndpoint) -> Result<(), SystemError> {
        if self.addr.is_some() {
            log::error!("the socket is already bound");
            return endpoint_to_bind.bind_unnamed();
        }

        let bound_addr = endpoint_to_bind.bind()?;
        self.addr = Some(bound_addr);

        Ok(())
    }

    pub(super) fn into_connected(self, peer_addr: UnixEndpointBound) -> (Connected, Connected) {
        let Init { addr } = self;
        Connected::new_pair(addr, Some(peer_addr))
    }

    pub(super) fn endpoint(&self) -> Option<Endpoint> {
        self.addr.clone().map(|addr| addr.into())
    }

    pub(super) fn check_io_events(&self) -> EPollEventType {
        EPollEventType::EPOLLHUP | EPollEventType::EPOLLOUT
    }

    pub(super) fn listen(
        self,
        backlog: usize,
        is_seqpacket: bool,
        wait_queue: Arc<WaitQueue>,
    ) -> Result<Listener, (SystemError, Self)> {
        let Some(addr) = self.addr else {
            return Err((SystemError::EINVAL, self));
        };

        Ok(Listener::new(addr, backlog, is_seqpacket, wait_queue))
    }
}

#[derive(Debug)]
pub struct Connected {
    addr: Option<UnixEndpointBound>,
    peer_addr: Option<UnixEndpointBound>,
    reader: SpinLock<RbConsumer<u8>>,
    writer: SpinLock<RbProducer<u8>>,
}

impl Connected {
    pub(super) fn new_pair(
        addr: Option<UnixEndpointBound>,
        peer_addr: Option<UnixEndpointBound>,
    ) -> (Self, Self) {
        let (this_writer, peer_reader) = RingBuffer::new(UNIX_STREAM_DEFAULT_BUF_SIZE).split();
        let (peer_writer, this_reader) = RingBuffer::new(UNIX_STREAM_DEFAULT_BUF_SIZE).split();

        let this = Connected {
            addr: addr.clone(),
            peer_addr: peer_addr.clone(),
            reader: SpinLock::new(this_reader),
            writer: SpinLock::new(this_writer),
        };
        let peer = Connected {
            addr: peer_addr,
            peer_addr: addr,
            reader: SpinLock::new(peer_reader),
            writer: SpinLock::new(peer_writer),
        };

        return (this, peer);
    }

    pub(super) fn endpoint(&self) -> Endpoint {
        self.addr.clone().unwrap().into()
    }

    pub(super) fn set_endpoint(&mut self, addr: Option<UnixEndpointBound>) {
        self.addr = addr;
    }

    pub(super) fn peer_endpoint(&self) -> Option<UnixEndpointBound> {
        self.peer_addr.clone()
    }

    #[allow(dead_code)]
    pub(super) fn set_peer_endpoint(&mut self, peer: Option<UnixEndpointBound>) {
        self.peer_addr = peer;
    }

    pub(super) fn bind(&mut self, addr_to_bind: UnixEndpoint) -> Result<(), SystemError> {
        if self.addr.is_some() {
            return addr_to_bind.bind_unnamed();
        }

        let bound_addr = addr_to_bind.bind()?;
        self.set_endpoint(Some(bound_addr));

        Ok(())
    }

    pub(super) fn try_send(&self, buf: &[u8], is_seqpacket: bool) -> Result<usize, SystemError> {
        let is_empty = buf.is_empty();
        if is_empty {
            //todo 判断shutdown
            if !is_seqpacket {
                return Ok(0);
            }
        }

        // shutdown(2) semantics:
        // - If this end has SHUT_WR, sending must fail with EPIPE.
        // - If peer has SHUT_RD, sending must fail with EPIPE.
        {
            let guard = self.writer.lock();
            if guard.is_send_shutdown() || guard.is_recv_shutdown() {
                return Err(SystemError::EPIPE);
            }
        }

        if is_seqpacket && buf.len() > UNIX_STREAM_DEFAULT_BUF_SIZE {
            return Err(SystemError::EMSGSIZE);
        }

        //todo 判断辅助数据
        let buffer = if is_seqpacket {
            let mut buffer = Vec::with_capacity(buf.len() + 4);
            let len = buf.len() as u32;
            buffer.extend_from_slice(&len.to_ne_bytes());
            buffer.extend_from_slice(buf);
            buffer
        } else {
            buf.to_vec()
        };
        let mut guard = self.writer.lock();
        let can_send = guard.free_len() >= buffer.len();

        // log::info!("Going to send {} bytes", buffer.len());
        if can_send {
            guard.push_slice(&buffer);
        } else {
            log::debug!("can not send {:?}", String::from_utf8_lossy(buf));
            return Err(SystemError::ENOBUFS);
        }

        Ok(buf.len())
    }

    pub fn try_recv(&self, buf: &mut [u8], is_seqpacket: bool) -> Result<usize, SystemError> {
        if is_seqpacket {
            {
                let guard = self.reader.lock();
                if guard.len() < size_of::<u32>() {
                    // If peer has SHUT_WR and the receive queue is empty, return EOF.
                    if guard.is_send_shutdown() {
                        return Ok(0);
                    }
                    return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
                }
            }

            let mut len_buf = [0u8; 4];
            self.reader.lock().pop_slice(&mut len_buf);
            let len = u32::from_ne_bytes(len_buf) as usize;

            if len == 0 {
                return Ok(0);
            }

            if buf.len() < len {
                return Err(SystemError::EMSGSIZE);
            }

            if self.reader.lock().len() < len {
                return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
            }

            self.reader.lock().pop_slice(&mut buf[..len]);
            Ok(len)
        } else {
            let avail_len = {
                let guard = self.reader.lock();
                let len = guard.len();
                if len == 0 {
                    // If peer has SHUT_WR and the receive queue is empty, return EOF.
                    if guard.is_send_shutdown() {
                        return Ok(0);
                    }
                    return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
                }
                len
            };
            let len = core::cmp::min(buf.len(), avail_len);
            self.reader.lock().pop_slice(&mut buf[..len]);
            Ok(len)
        }
    }

    pub(super) fn readable_len(&self, is_seqpacket: bool) -> usize {
        if is_seqpacket {
            let guard = self.reader.lock();
            if guard.len() < size_of::<u32>() {
                return 0;
            }

            let mut len_buf = [0u8; 4];
            if guard.peek_slice(&mut len_buf).is_none() {
                return 0;
            }

            let len = u32::from_ne_bytes(len_buf) as usize;
            // If the header is present but the payload isn't fully queued yet,
            // report 0 (can't read a full record without blocking).
            if guard.len() < size_of::<u32>() + len {
                return 0;
            }

            len
        } else {
            self.reader.lock().len()
        }
    }

    pub(super) fn outq_len(&self, _is_seqpacket: bool) -> usize {
        // Approximation: bytes sitting in the local-to-peer ring buffer,
        // i.e. written but not yet consumed by the peer.
        self.writer.lock().len()
    }

    pub(super) fn shutdown_recv(&self) {
        self.reader.lock().set_recv_shutdown();
    }

    pub(super) fn shutdown_send(&self) {
        self.writer.lock().set_send_shutdown();
    }

    pub(super) fn push_scm_rights(&self, files: Vec<Arc<File>>) {
        self.writer.lock().push_scm_rights(files)
    }

    pub(super) fn pop_scm_rights(&self) -> Option<Vec<Arc<File>>> {
        self.reader.lock().pop_scm_rights()
    }

    pub(super) fn check_io_events(&self) -> EPollEventType {
        let mut events = EPollEventType::empty();

        if !self.reader.lock().is_empty() {
            events |= EPollEventType::EPOLLIN;
        }
        if !self.writer.lock().is_empty() {
            events |= EPollEventType::EPOLLOUT;
        }

        events
    }
}

#[derive(Debug)]
pub(super) struct Listener {
    backlog: Arc<Backlog>,
}

impl Listener {
    pub(super) fn new(
        addr: UnixEndpointBound,
        backlog: usize,
        is_seqpacket: bool,
        wait_queue: Arc<WaitQueue>,
    ) -> Self {
        let backlog = BACKLOG_TABLE
            .add_backlog(addr, backlog, is_seqpacket, false, wait_queue)
            .unwrap();

        Self { backlog }
    }

    pub(super) fn endpoint(&self) -> Endpoint {
        self.backlog.addr().clone().into()
    }

    pub fn listen(&self, backlog: usize) {
        self.backlog.set_backlog(backlog);
    }

    pub(super) fn try_accept(
        &self,
        is_seqpacket: bool,
    ) -> Result<(Arc<dyn Socket>, Endpoint), SystemError> {
        let connected = self.backlog.pop_incoming()?;

        let peer_addr = connected.peer_endpoint().into();
        let socket = UnixStreamSocket::new_connected(connected, false, is_seqpacket);

        Ok((socket, peer_addr))
    }

    pub(super) fn check_io_events(&self) -> EPollEventType {
        self.backlog.check_io_events()
    }

    pub(super) fn is_acceptable(&self) -> bool {
        !self
            .backlog
            .incoming_conns
            .lock()
            .as_ref()
            .unwrap_or(&VecDeque::new())
            .is_empty()
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        unregister_backlog(self.backlog.addr())
    }
}

struct BacklogTable {
    backlog_sockets: RwLock<BTreeMap<UnixEndpointBound, Arc<Backlog>>>,
}

impl BacklogTable {
    const fn new() -> Self {
        Self {
            backlog_sockets: RwLock::new(BTreeMap::new()),
        }
    }

    fn add_backlog(
        &self,
        addr: UnixEndpointBound,
        backlog: usize,
        is_seqpacket: bool,
        is_shutdown: bool,
        wait_queue: Arc<WaitQueue>,
    ) -> Option<Arc<Backlog>> {
        let mut guard = self.backlog_sockets.write();
        if guard.contains_key(&addr) {
            return None;
        }

        let new_backlog = Arc::new(Backlog::new(
            addr.clone(),
            backlog,
            is_seqpacket,
            is_shutdown,
            wait_queue,
        ));
        guard.insert(addr, new_backlog.clone());

        Some(new_backlog)
    }

    fn get_backlog(&self, addr: &UnixEndpointBound) -> Option<Arc<Backlog>> {
        self.backlog_sockets.read().get(addr).cloned()
    }

    fn remove_backlog(&self, addr: &UnixEndpointBound) {
        self.backlog_sockets.write().remove(addr);
    }
}

static BACKLOG_TABLE: BacklogTable = BacklogTable::new();

#[derive(Debug)]
pub(super) struct Backlog {
    addr: UnixEndpointBound,
    backlog: AtomicUsize,
    incoming_conns: SpinLock<Option<VecDeque<Connected>>>,
    wait_queue: Arc<WaitQueue>,
    is_seqpacket: bool,
    _is_shutdown: bool,
}

impl Backlog {
    fn new(
        addr: UnixEndpointBound,
        backlog: usize,
        is_seqpacket: bool,
        is_shutdown: bool,
        wait_queue: Arc<WaitQueue>,
    ) -> Self {
        let incoming_sockets = if is_shutdown {
            None
        } else {
            Some(VecDeque::with_capacity(backlog))
        };

        Self {
            addr,
            backlog: AtomicUsize::new(backlog),
            incoming_conns: SpinLock::new(incoming_sockets),
            wait_queue,
            is_seqpacket,
            _is_shutdown: is_shutdown,
        }
    }

    fn addr(&self) -> &UnixEndpointBound {
        &self.addr
    }

    fn pop_incoming(&self) -> Result<Connected, SystemError> {
        let mut guard = self.incoming_conns.lock();

        let Some(incoming_conns) = &mut *guard else {
            return Err(SystemError::EINVAL);
        };
        let conn = incoming_conns.pop_front();
        drop(guard);

        conn.ok_or(SystemError::EAGAIN_OR_EWOULDBLOCK)
    }

    fn set_backlog(&self, backlog: usize) {
        let old_backlog = self.backlog.swap(backlog, Ordering::Relaxed);

        if old_backlog < backlog {
            self.wait_queue
                .wakeup(Some(crate::process::ProcessState::Blocked(true)));
        }
    }

    // fn is_shutdown(&self) -> bool {
    //     self.is_shutdown
    // }

    // fn shutdown(&self) {
    //     *self.incoming_conns.lock() = None;
    //     self.wait_queue
    //         .wakeup_all(Some(crate::process::ProcessState::Blocked(true)));
    // }

    fn check_io_events(&self) -> EPollEventType {
        if self
            .incoming_conns
            .lock()
            .as_ref()
            .is_some_and(|conns| !conns.is_empty())
        {
            EPollEventType::EPOLLIN
        } else {
            EPollEventType::empty()
        }
    }

    pub(super) fn push_incoming(
        &self,
        init: Init,
        is_seqpacket: bool,
    ) -> Result<Connected, (Init, SystemError)> {
        if is_seqpacket != self.is_seqpacket {
            //todo 这里应该是专门为sock_stream和socket_seqpacket分别创建两个socket table
            return Err((init, SystemError::ECONNREFUSED));
        }
        let mut guard = self.incoming_conns.lock();

        let Some(incoming_conns) = &mut *guard else {
            return Err((init, SystemError::EINVAL));
        };

        if incoming_conns.len() >= self.backlog.load(Ordering::Relaxed) {
            debug!("the pending connection queue on the listening socket is full");
            return Err((init, SystemError::EAGAIN_OR_EWOULDBLOCK));
        }

        let (client_conn, server_conn) = init.into_connected(self.addr.clone());
        incoming_conns.push_back(server_conn);
        self.wait_queue
            .wakeup(Some(crate::process::ProcessState::Blocked(true)));
        Ok(client_conn)
    }

    pub(super) fn pause_until<F>(&self, mut cond: F) -> Result<(), SystemError>
    where
        F: FnMut() -> Result<(), SystemError>,
    {
        wq_wait_event_interruptible!(
            self.wait_queue,
            match cond() {
                Err(e) if e.eq(&SystemError::EAGAIN_OR_EWOULDBLOCK) => false,
                _res => true,
            },
            {}
        )
    }
}

fn unregister_backlog(addr: &UnixEndpointBound) {
    BACKLOG_TABLE.remove_backlog(addr);
}

pub(super) fn get_backlog(server_key: &UnixEndpointBound) -> Result<Arc<Backlog>, SystemError> {
    BACKLOG_TABLE
        .get_backlog(server_key)
        .ok_or(SystemError::ECONNREFUSED)
}
