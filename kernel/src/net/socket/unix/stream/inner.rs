use crate::filesystem::epoll::EPollEventType;
use crate::filesystem::vfs::file::File;
use crate::libs::rwlock::RwLock;
use crate::libs::spinlock::SpinLock;
use crate::libs::wait_queue::WaitQueue;
use crate::net::socket::endpoint::Endpoint;
use crate::net::socket::unix::ring_buffer::{RbConsumer, RbProducer, RingBuffer};
use crate::net::socket::unix::stream::UnixStreamSocket;
use crate::net::socket::unix::UCred;
use crate::net::socket::unix::{UnixEndpoint, UnixEndpointBound};
use crate::net::socket::Socket;
use crate::process::namespace::net_namespace::NetNamespace;
use alloc::collections::BTreeMap;
use alloc::collections::VecDeque;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::mem::size_of;
use core::num::Wrapping;
use core::sync::atomic::{AtomicUsize, Ordering};
use log::debug;
use system_error::SystemError;

pub(in crate::net) const UNIX_STREAM_DEFAULT_BUF_SIZE: usize = 65536;

/// SCM snapshot for recvmsg containing ancillary data
pub(super) struct ScmSnapshot {
    /// SCM data at head (optional credentials and file rights)
    pub(super) scm_data: Option<(Option<UCred>, Vec<Arc<File>>)>,
}

#[derive(Debug, Clone)]
pub(super) struct StreamRecvmsgMeta {
    pub(super) copy_len: usize,
    pub(super) scm_cred: Option<UCred>,
    pub(super) scm_rights: Vec<Arc<File>>,
}

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

    pub(super) fn bind(
        &mut self,
        endpoint_to_bind: UnixEndpoint,
        netns: &Arc<NetNamespace>,
    ) -> Result<(), SystemError> {
        if self.addr.is_some() {
            log::error!("the socket is already bound");
            return endpoint_to_bind.bind_unnamed();
        }

        let bound_addr = endpoint_to_bind.bind_in(netns)?;
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
        sndbuf_effective: usize,
        rcvbuf_effective: usize,
        netns: Arc<NetNamespace>,
    ) -> Result<Listener, (SystemError, Self)> {
        let Some(addr) = self.addr else {
            return Err((SystemError::EINVAL, self));
        };

        Ok(Listener::new(
            addr,
            backlog,
            is_seqpacket,
            wait_queue,
            sndbuf_effective,
            rcvbuf_effective,
            netns,
        ))
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
        // Client sockets may connect without bind(2); getsockname(2) must not panic.
        self.addr
            .clone()
            .map(Into::into)
            .unwrap_or(Endpoint::Unix(UnixEndpoint::Unnamed))
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

    pub(super) fn bind(
        &mut self,
        addr_to_bind: UnixEndpoint,
        netns: &Arc<NetNamespace>,
    ) -> Result<(), SystemError> {
        if self.addr.is_some() {
            return addr_to_bind.bind_unnamed();
        }

        let bound_addr = addr_to_bind.bind_in(netns)?;
        self.set_endpoint(Some(bound_addr));

        Ok(())
    }

    pub(super) fn resize_sendbuf(&self, new_capacity: usize) -> Result<(), SystemError> {
        self.writer.lock().resize(new_capacity)
    }

    pub(super) fn resize_recvbuf(&self, new_capacity: usize) -> Result<(), SystemError> {
        self.reader.lock().resize(new_capacity)
    }

    pub(super) fn send_capacity(&self) -> usize {
        self.writer.lock().capacity()
    }

    pub(super) fn recv_capacity(&self) -> usize {
        self.reader.lock().capacity()
    }

    pub(super) fn set_connreset_to_peer(&self) {
        self.writer.lock().set_connreset_pending();
    }

    pub(super) fn take_connreset_from_peer(&self) -> bool {
        self.reader.lock().take_connreset_pending()
    }

    pub(super) fn send_free_len(&self) -> usize {
        self.writer.lock().free_len()
    }

    pub(super) fn try_send(
        &self,
        buf: &[u8],
        is_seqpacket: bool,
        sndbuf_limit: usize,
    ) -> Result<(usize, Wrapping<usize>, usize), SystemError> {
        let is_empty = buf.is_empty();
        if is_empty {
            //todo 判断shutdown
            if !is_seqpacket {
                return Ok((0, Wrapping(0), 0));
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

        // SO_SNDBUF accounting (Linux-like, approximate): fail with ENOBUFS when
        // the amount of queued data would exceed the configured send buffer.
        // This is required by gVisor stream unix socket tests.
        let queued = guard.len();
        if queued.saturating_add(buffer.len()) > sndbuf_limit {
            return Err(SystemError::ENOBUFS);
        }

        let start = guard.tail();
        let can_send = guard.free_len() >= buffer.len();

        // log::info!("Going to send {} bytes", buffer.len());
        if can_send {
            guard.push_slice(&buffer);
        } else {
            return Err(SystemError::ENOBUFS);
        }

        Ok((buf.len(), start, buffer.len()))
    }

    pub fn try_recv(&self, buf: &mut [u8], is_seqpacket: bool) -> Result<usize, SystemError> {
        if is_seqpacket {
            let (copy_len, _orig_len, _truncated) = self.try_recv_seqpacket_meta(buf, false)?;
            Ok(copy_len)
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

    pub fn try_peek(&self, buf: &mut [u8], is_seqpacket: bool) -> Result<usize, SystemError> {
        if is_seqpacket {
            let (copy_len, _orig_len, _truncated) = self.try_recv_seqpacket_meta(buf, true)?;
            Ok(copy_len)
        } else {
            let guard = self.reader.lock();
            let avail_len = guard.len();
            if avail_len == 0 {
                if guard.is_send_shutdown() {
                    return Ok(0);
                }
                return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
            }
            let len = core::cmp::min(buf.len(), avail_len);
            if guard.peek_slice(&mut buf[..len]).is_none() {
                return Err(SystemError::EFAULT);
            }
            Ok(len)
        }
    }

    /// Receive exactly one SOCK_SEQPACKET record.
    ///
    /// Returns `(copy_len, orig_len, truncated)`.
    /// - `copy_len` is the number of bytes copied into `buf`.
    /// - `orig_len` is the record's original payload length.
    /// - `truncated` is true if `buf` was smaller than the record.
    ///
    /// If `peek` is true, the record is not consumed.
    pub fn try_recv_seqpacket_meta(
        &self,
        buf: &mut [u8],
        peek: bool,
    ) -> Result<(usize, usize, bool), SystemError> {
        let mut guard = self.reader.lock();
        if guard.len() < size_of::<u32>() {
            // If peer has SHUT_WR and the receive queue is empty, return EOF.
            if guard.is_send_shutdown() {
                return Ok((0, 0, false));
            }
            return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
        }

        let mut len_buf = [0u8; 4];
        if guard.peek_slice(&mut len_buf).is_none() {
            return Err(SystemError::EFAULT);
        }
        let record_len = u32::from_ne_bytes(len_buf) as usize;

        if guard.len() < size_of::<u32>() + record_len {
            return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
        }

        let copy_len = core::cmp::min(buf.len(), record_len);
        let truncated = copy_len < record_len;

        if peek {
            if copy_len != 0 {
                let payload_off = guard.head() + Wrapping(size_of::<u32>());
                if guard
                    .peek_slice_at(payload_off, &mut buf[..copy_len])
                    .is_none()
                {
                    return Err(SystemError::EFAULT);
                }
            }
            return Ok((copy_len, record_len, truncated));
        }

        // Consume header.
        guard.pop_slice(&mut len_buf);
        if record_len == 0 {
            return Ok((0, 0, false));
        }

        if copy_len != 0 {
            guard.pop_slice(&mut buf[..copy_len]);
        }

        // Discard remaining bytes of this record.
        let mut remaining = record_len - copy_len;
        if remaining != 0 {
            let mut trash = [0u8; 256];
            while remaining != 0 {
                let chunk = core::cmp::min(remaining, trash.len());
                guard.pop_slice(&mut trash[..chunk]);
                remaining -= chunk;
            }
        }

        Ok((copy_len, record_len, truncated))
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

    pub(super) fn peer_send_shutdown(&self) -> bool {
        self.reader.lock().is_send_shutdown()
    }

    pub(super) fn push_scm_at(
        &self,
        offset: Wrapping<usize>,
        len: usize,
        cred: Option<UCred>,
        rights: Vec<Arc<File>>,
    ) {
        self.writer.lock().push_scm_at(offset, len, cred, rights)
    }

    #[allow(dead_code)]
    pub(super) fn peek_scm_at_head(&self) -> Option<(Option<UCred>, Vec<Arc<File>>)> {
        let guard = self.reader.lock();
        let head = guard.head();
        guard.peek_scm_at(head)
    }

    #[allow(dead_code)]
    pub(super) fn read_head(&self) -> Wrapping<usize> {
        self.reader.lock().head()
    }

    #[allow(dead_code)]
    pub(super) fn next_scm_offset_after_head(&self) -> Option<Wrapping<usize>> {
        let guard = self.reader.lock();
        let head = guard.head();
        guard.next_scm_offset_after(head)
    }

    pub(super) fn scm_snapshot_for_recvmsg(&self) -> ScmSnapshot {
        let guard = self.reader.lock();
        let scm_data = guard.peek_scm_at(guard.head());
        ScmSnapshot { scm_data }
    }

    pub(super) fn try_recv_stream_recvmsg_meta(
        &self,
        buf: &mut [u8],
        peek: bool,
        want_creds: bool,
    ) -> Result<StreamRecvmsgMeta, SystemError> {
        let mut guard = self.reader.lock();

        let avail_len = guard.len();
        if avail_len == 0 {
            if guard.is_send_shutdown() {
                return Ok(StreamRecvmsgMeta {
                    copy_len: 0,
                    scm_cred: None,
                    scm_rights: Vec::new(),
                });
            }
            return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
        }

        let max = core::cmp::min(buf.len(), avail_len);
        let plan = guard.plan_stream_recvmsg(max, want_creds);
        let n = plan.bytes;

        if n != 0 {
            if peek {
                if guard.peek_slice(&mut buf[..n]).is_none() {
                    return Err(SystemError::EFAULT);
                }
            } else {
                if guard.pop_slice_preserve_records(&mut buf[..n]).is_none() {
                    return Err(SystemError::EFAULT);
                }

                // Once any bytes of the rights-carrying record are consumed via
                // recvmsg, rights are either delivered or discarded, but must not
                // be visible again.
                if let Some(start) = plan.rights_start {
                    guard.clear_rights_at(start);
                }
            }
        }

        Ok(StreamRecvmsgMeta {
            copy_len: n,
            scm_cred: plan.cred,
            scm_rights: plan.rights,
        })
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
        sndbuf_effective: usize,
        rcvbuf_effective: usize,
        netns: Arc<NetNamespace>,
    ) -> Self {
        let params = BacklogParams {
            addr,
            backlog,
            is_seqpacket,
            is_shutdown: false,
            wait_queue,
            sndbuf_effective,
            rcvbuf_effective,
            netns,
        };
        let backlog = BACKLOG_TABLE.add_backlog(params).unwrap();

        Self { backlog }
    }

    pub(super) fn endpoint(&self) -> Endpoint {
        self.backlog.addr().clone().into()
    }

    pub fn listen(&self, backlog: usize) {
        self.backlog.set_backlog(backlog);
    }

    pub(super) fn set_sndbuf_effective(&self, effective: usize) {
        self.backlog.set_sndbuf_effective(effective);
    }

    pub(super) fn set_rcvbuf_effective(&self, effective: usize) {
        self.backlog.set_rcvbuf_effective(effective);
    }

    pub(super) fn try_accept(
        &self,
        inherit_passcred: bool,
    ) -> Result<(Arc<dyn Socket>, Endpoint), SystemError> {
        let socket = self.backlog.pop_incoming()?;

        let peer_addr = match socket.inner.read().as_ref().expect("inner is None") {
            Inner::Connected(connected) => connected.peer_endpoint().into(),
            _ => Endpoint::Unix(UnixEndpoint::Unnamed),
        };

        socket
            .passcred
            .store(inherit_passcred, core::sync::atomic::Ordering::Relaxed);

        Ok((socket, peer_addr))
    }

    pub(super) fn check_io_events(&self) -> EPollEventType {
        self.backlog.check_io_events()
    }

    pub(super) fn is_acceptable(&self) -> bool {
        self.backlog
            .incoming_conns
            .lock()
            .as_ref()
            .is_some_and(|q| !q.is_empty())
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

    fn add_backlog(&self, params: BacklogParams) -> Option<Arc<Backlog>> {
        let mut guard = self.backlog_sockets.write();
        if guard.contains_key(&params.addr) {
            return None;
        }

        let addr = params.addr.clone();
        let new_backlog = Arc::new(Backlog::new(params));
        guard.insert(addr, new_backlog.clone());

        Some(new_backlog)
    }

    fn get_backlog(&self, addr: &UnixEndpointBound) -> Option<Arc<Backlog>> {
        self.backlog_sockets.read().get(addr).cloned()
    }

    fn remove_backlog(&self, addr: &UnixEndpointBound) -> Option<Arc<Backlog>> {
        self.backlog_sockets.write().remove(addr)
    }
}

static BACKLOG_TABLE: BacklogTable = BacklogTable::new();

/// Parameters for creating a new backlog entry
#[derive(Clone)]
struct BacklogParams {
    addr: UnixEndpointBound,
    backlog: usize,
    is_seqpacket: bool,
    is_shutdown: bool,
    wait_queue: Arc<WaitQueue>,
    sndbuf_effective: usize,
    rcvbuf_effective: usize,
    netns: Arc<NetNamespace>,
}

#[derive(Debug)]
pub(super) struct Backlog {
    addr: UnixEndpointBound,
    backlog: AtomicUsize,
    sndbuf_effective: AtomicUsize,
    rcvbuf_effective: AtomicUsize,
    incoming_conns: SpinLock<Option<VecDeque<Arc<UnixStreamSocket>>>>,
    wait_queue: Arc<WaitQueue>,
    is_seqpacket: bool,
    netns: Arc<NetNamespace>,
    _is_shutdown: bool,
}

impl Backlog {
    fn new(params: BacklogParams) -> Self {
        let incoming_sockets = if params.is_shutdown {
            None
        } else {
            Some(VecDeque::with_capacity(params.backlog))
        };

        Self {
            addr: params.addr,
            backlog: AtomicUsize::new(params.backlog),
            sndbuf_effective: AtomicUsize::new(params.sndbuf_effective),
            rcvbuf_effective: AtomicUsize::new(params.rcvbuf_effective),
            incoming_conns: SpinLock::new(incoming_sockets),
            wait_queue: params.wait_queue,
            is_seqpacket: params.is_seqpacket,
            netns: params.netns,
            _is_shutdown: params.is_shutdown,
        }
    }

    fn sndbuf_effective(&self) -> usize {
        self.sndbuf_effective.load(Ordering::Relaxed)
    }

    fn rcvbuf_effective(&self) -> usize {
        self.rcvbuf_effective.load(Ordering::Relaxed)
    }

    fn set_sndbuf_effective(&self, effective: usize) {
        self.sndbuf_effective.store(effective, Ordering::Relaxed);
    }

    fn set_rcvbuf_effective(&self, effective: usize) {
        self.rcvbuf_effective.store(effective, Ordering::Relaxed);
    }

    fn addr(&self) -> &UnixEndpointBound {
        &self.addr
    }

    fn pop_incoming(&self) -> Result<Arc<UnixStreamSocket>, SystemError> {
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

    fn shutdown_pending(&self) {
        let pending = {
            let mut guard = self.incoming_conns.lock();
            guard.take()
        };

        let Some(mut q) = pending else {
            return;
        };

        while let Some(sock) = q.pop_front() {
            let _ = sock.do_close();
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
        client_socket: Arc<UnixStreamSocket>,
        is_seqpacket: bool,
        client_sndbuf_effective: usize,
        client_rcvbuf_effective: usize,
    ) -> Result<Connected, (Init, SystemError)> {
        if is_seqpacket != self.is_seqpacket {
            //todo 这里应该是专门为sock_stream和socket_seqpacket分别创建两个socket table
            return Err((init, SystemError::ECONNREFUSED));
        }
        let mut guard = self.incoming_conns.lock();

        let Some(incoming_conns) = &mut *guard else {
            return Err((init, SystemError::EINVAL));
        };

        // Linux uses sk_acceptq_is_full(): ack_backlog > max_ack_backlog.
        // This means backlog==0 still allows one pending connection.
        if incoming_conns.len() > self.backlog.load(Ordering::Relaxed) {
            debug!("the pending connection queue on the listening socket is full");
            return Err((init, SystemError::EAGAIN_OR_EWOULDBLOCK));
        }

        let (client_conn, server_conn) = init.into_connected(self.addr.clone());

        // Apply buffer sizes that may have been configured before connect.
        let client_snd_cap = super::ring_cap_for_effective_sockbuf(client_sndbuf_effective);
        let client_rcv_cap = super::ring_cap_for_effective_sockbuf(client_rcvbuf_effective);
        let server_snd_cap = super::ring_cap_for_effective_sockbuf(self.sndbuf_effective());
        let server_rcv_cap = super::ring_cap_for_effective_sockbuf(self.rcvbuf_effective());

        // Each direction is backed by a *shared* ring buffer between:
        // - client writer <-> server reader
        // - server writer <-> client reader
        // Pick a capacity that satisfies both endpoints for that direction.
        let c2s_cap = core::cmp::max(client_snd_cap, server_rcv_cap);
        let s2c_cap = core::cmp::max(server_snd_cap, client_rcv_cap);

        let _ = client_conn.resize_sendbuf(c2s_cap);
        let _ = server_conn.resize_recvbuf(c2s_cap);
        let _ = server_conn.resize_sendbuf(s2c_cap);
        let _ = client_conn.resize_recvbuf(s2c_cap);

        let server_socket =
            UnixStreamSocket::new_connected(server_conn, false, is_seqpacket, self.netns.clone());

        // Listener-side buffer settings.
        server_socket
            .sndbuf
            .store(self.sndbuf_effective(), Ordering::Relaxed);
        server_socket
            .rcvbuf
            .store(self.rcvbuf_effective(), Ordering::Relaxed);

        // Wire up peer pointers so recv can wake blocked senders and we can
        // deliver EPOLL/SIGIO notifications correctly.
        *client_socket.peer.lock() = Some(Arc::downgrade(&server_socket));
        *server_socket.peer.lock() = Some(Arc::downgrade(&client_socket));

        incoming_conns.push_back(server_socket);
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
    if let Some(backlog) = BACKLOG_TABLE.remove_backlog(addr) {
        backlog.shutdown_pending();
    }
}

pub(super) fn get_backlog(server_key: &UnixEndpointBound) -> Result<Arc<Backlog>, SystemError> {
    BACKLOG_TABLE
        .get_backlog(server_key)
        .ok_or(SystemError::ECONNREFUSED)
}
