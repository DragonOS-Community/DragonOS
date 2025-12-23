use crate::{
    filesystem::{
        epoll::EPollEventType,
        vfs::{fasync::FAsyncItems, vcore::generate_inode_id, InodeId},
    },
    libs::rwlock::RwLock,
    net::socket::{self, *},
};
use crate::{
    libs::spinlock::SpinLock,
    libs::wait_queue::WaitQueue,
    net::{
        posix::MsgHdr,
        socket::{
            common::EPollItems,
            endpoint::Endpoint,
            unix::{
                stream::inner::{get_backlog, Backlog},
                UnixEndpoint,
            },
        },
    },
};
use alloc::sync::{Arc, Weak};
use core::sync::atomic::AtomicBool;
use inner::{Connected, Init, Inner};
use log::debug;
use system_error::SystemError;

pub mod inner;

#[cast_to([sync] Socket)]
#[derive(Debug)]
pub struct UnixStreamSocket {
    inner: RwLock<Option<Inner>>,
    //todo options
    epitems: EPollItems,
    fasync_items: FAsyncItems,
    wait_queue: Arc<WaitQueue>,
    inode_id: InodeId,
    /// Peer socket for socket pairs (used for SIGIO notification)
    peer: SpinLock<Option<Weak<UnixStreamSocket>>>,

    is_nonblocking: AtomicBool,
    is_seqpacket: bool,
}

impl UnixStreamSocket {
    /// 默认的元数据缓冲区大小
    #[allow(dead_code)]
    pub const DEFAULT_METADATA_BUF_SIZE: usize = 1024;
    /// 默认的缓冲区大小
    pub const DEFAULT_BUF_SIZE: usize = 64 * 1024;

    pub(super) fn new_init(init: Init, is_nonblocking: bool, is_seqpacket: bool) -> Arc<Self> {
        Arc::new(Self {
            inner: RwLock::new(Some(Inner::Init(init))),
            wait_queue: Arc::new(WaitQueue::default()),
            inode_id: generate_inode_id(),
            is_nonblocking: AtomicBool::new(is_nonblocking),
            is_seqpacket,
            epitems: EPollItems::default(),
            fasync_items: FAsyncItems::default(),
            peer: SpinLock::new(None),
        })
    }

    pub(super) fn new_connected(
        connected: Connected,
        is_nonblocking: bool,
        is_seqpacket: bool,
    ) -> Arc<Self> {
        Arc::new(Self {
            inner: RwLock::new(Some(Inner::Connected(connected))),
            wait_queue: Arc::new(WaitQueue::default()),
            inode_id: generate_inode_id(),
            is_nonblocking: AtomicBool::new(is_nonblocking),
            is_seqpacket,
            epitems: EPollItems::default(),
            fasync_items: FAsyncItems::default(),
            peer: SpinLock::new(None),
        })
    }

    pub fn new(is_nonblocking: bool, is_seqpacket: bool) -> Arc<Self> {
        Self::new_init(Init::new(), is_nonblocking, is_seqpacket)
    }

    pub fn new_pair(is_nonblocking: bool, is_seqpacket: bool) -> (Arc<Self>, Arc<Self>) {
        let (conn_a, conn_b) = Connected::new_pair(None, None);
        let socket_a = Self::new_connected(conn_a, is_nonblocking, is_seqpacket);
        let socket_b = Self::new_connected(conn_b, is_nonblocking, is_seqpacket);

        // Set up peer references for SIGIO notification
        *socket_a.peer.lock() = Some(Arc::downgrade(&socket_b));
        *socket_b.peer.lock() = Some(Arc::downgrade(&socket_a));

        (socket_a, socket_b)
    }

    fn try_send(&self, buffer: &[u8]) -> Result<usize, SystemError> {
        match self.inner.read().as_ref().expect("inner is None") {
            Inner::Connected(connected) => connected.try_send(buffer, self.is_seqpacket),
            _ => {
                log::error!("the socket is not connected");
                return Err(SystemError::ENOTCONN);
            }
        }
    }

    fn try_recv(&self, buffer: &mut [u8]) -> Result<usize, SystemError> {
        match self.inner.read().as_ref().expect("inner is None") {
            Inner::Connected(connected) => connected.try_recv(buffer, self.is_seqpacket),
            _ => {
                log::error!("the socket is not connected");
                return Err(SystemError::ENOTCONN);
            }
        }
    }

    fn try_connect(&self, backlog: &Arc<Backlog>) -> Result<(), SystemError> {
        let mut writer = self.inner.write();
        let inner = writer.take().expect("inner is None");

        let (inner, result) = match inner {
            Inner::Init(init) => match backlog.push_incoming(init, self.is_seqpacket) {
                Ok(connected) => (Inner::Connected(connected), Ok(())),
                Err((init, err)) => (Inner::Init(init), Err(err)),
            },
            Inner::Listener(inner) => (Inner::Listener(inner), Err(SystemError::EINVAL)),
            Inner::Connected(connected) => (Inner::Connected(connected), Err(SystemError::EISCONN)),
        };

        match result {
            Ok(()) | Err(SystemError::EINPROGRESS) => {}
            _ => {}
        }

        writer.replace(inner);

        result
    }

    pub fn try_accept(&self) -> Result<(Arc<dyn Socket>, Endpoint), SystemError> {
        match self.inner.write().as_mut().expect("inner is None") {
            Inner::Listener(listener) => listener.try_accept(self.is_seqpacket) as _,
            _ => {
                log::error!("the socket is not listening");
                return Err(SystemError::EINVAL);
            }
        }
    }

    fn is_nonblocking(&self) -> bool {
        self.is_nonblocking
            .load(core::sync::atomic::Ordering::Relaxed)
    }

    fn is_acceptable(&self) -> bool {
        match self
            .inner
            .read()
            .as_ref()
            .expect("UnixStreamSocket inner is None")
        {
            Inner::Listener(listener) => listener.is_acceptable(),
            _ => false,
        }
    }

    /// Check if there's data available to receive
    fn can_recv(&self) -> bool {
        match self
            .inner
            .read()
            .as_ref()
            .expect("UnixStreamSocket inner is None")
        {
            Inner::Connected(connected) => connected
                .check_io_events()
                .contains(EPollEventType::EPOLLIN),
            _ => false,
        }
    }
}

impl Socket for UnixStreamSocket {
    fn connect(&self, server_endpoint: Endpoint) -> Result<(), SystemError> {
        let remote_addr = UnixEndpoint::try_from(server_endpoint)?.connect()?;
        let backlog = get_backlog(&remote_addr)?;

        if self.is_nonblocking() {
            self.try_connect(&backlog)
        } else {
            backlog.pause_until(|| self.try_connect(&backlog))
        }
    }

    fn bind(&self, endpoint: Endpoint) -> Result<(), SystemError> {
        let addr = UnixEndpoint::try_from(endpoint)?;

        let mut writer = self.inner.write();
        match writer.as_mut().expect("UnixStreamSocket inner is None") {
            Inner::Init(init) => init.bind(addr),
            Inner::Connected(connected) => connected.bind(addr),
            Inner::Listener(_listener) => addr.bind_unnamed(),
        }
    }

    fn listen(&self, backlog: usize) -> Result<(), SystemError> {
        const SOMAXCONN: usize = 4096;
        let backlog = backlog.saturating_add(1).min(SOMAXCONN);

        let mut writer = self.inner.write();

        let (inner, err) = match writer.take().expect("UnixStreamSocket inner is None") {
            Inner::Init(init) => {
                match init.listen(backlog, self.is_seqpacket, self.wait_queue.clone()) {
                    Ok(listener) => (Inner::Listener(listener), None),
                    Err((err, init)) => (Inner::Init(init), Some(err)),
                }
            }
            Inner::Listener(listener) => {
                listener.listen(backlog);
                (Inner::Listener(listener), None)
            }
            Inner::Connected(connected) => (Inner::Connected(connected), Some(SystemError::EINVAL)),
        };

        writer.replace(inner);
        drop(writer);

        if let Some(err) = err {
            return Err(err);
        }

        return Ok(());
    }

    fn accept(&self) -> Result<(Arc<dyn Socket>, Endpoint), SystemError> {
        debug!("stream server begin accept");
        if self.is_nonblocking() {
            self.try_accept()
        } else {
            loop {
                match self.try_accept() {
                    Err(SystemError::EAGAIN_OR_EWOULDBLOCK) => {
                        wq_wait_event_interruptible!(self.wait_queue, self.is_acceptable(), {})?
                    }
                    result => break result,
                }
            }
        }
    }

    fn set_option(&self, _level: PSOL, _optname: usize, _optval: &[u8]) -> Result<(), SystemError> {
        log::warn!("setsockopt is not implemented");
        Ok(())
    }

    fn wait_queue(&self) -> &WaitQueue {
        return &self.wait_queue;
    }

    fn local_endpoint(&self) -> Result<Endpoint, SystemError> {
        let addr: Endpoint = match self
            .inner
            .read()
            .as_ref()
            .expect("UnixStreamSocket inner is None")
        {
            Inner::Init(init) => init
                .endpoint()
                .unwrap_or(Endpoint::Unix(UnixEndpoint::Unnamed)),
            Inner::Connected(connected) => connected.endpoint(),
            Inner::Listener(listener) => listener.endpoint(),
        };

        Ok(addr)
    }

    fn remote_endpoint(&self) -> Result<Endpoint, SystemError> {
        let peer_addr = match self
            .inner
            .read()
            .as_ref()
            .expect("UnixStreamSocket inner is None")
        {
            Inner::Connected(connected) => connected.peer_endpoint(),
            _ => return Err(SystemError::ENOTCONN),
        };

        Ok(peer_addr.into())
    }

    fn recv(&self, buffer: &mut [u8], flags: socket::PMSG) -> Result<usize, SystemError> {
        // Check if non-blocking mode (either socket is non-blocking or DONTWAIT flag is set)
        let is_nonblocking = self.is_nonblocking() || flags.contains(PMSG::DONTWAIT);

        if is_nonblocking {
            self.try_recv(buffer)
        } else {
            // Blocking: wait until data is available
            loop {
                match self.try_recv(buffer) {
                    Ok(len) => return Ok(len),
                    Err(SystemError::EAGAIN_OR_EWOULDBLOCK) => {
                        wq_wait_event_interruptible!(self.wait_queue, self.can_recv(), {})?;
                    }
                    Err(e) => return Err(e),
                }
            }
        }
    }

    fn recv_from(
        &self,
        buffer: &mut [u8],
        flags: socket::PMSG,
        _address: Option<Endpoint>,
    ) -> Result<(usize, Endpoint), SystemError> {
        // OOB is not supported for Unix domain sockets
        if flags.contains(PMSG::OOB) {
            return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
        }

        // Get the peer endpoint (must be connected)
        let peer_endpoint = match self.inner.read().as_ref().expect("inner is None") {
            Inner::Connected(connected) => connected
                .peer_endpoint()
                .map(|addr| Endpoint::Unix(addr.into()))
                .unwrap_or(Endpoint::Unix(UnixEndpoint::Unnamed)),
            _ => return Err(SystemError::ENOTCONN),
        };

        // Check if non-blocking mode (either socket is non-blocking or DONTWAIT flag is set)
        let is_nonblocking = self.is_nonblocking() || flags.contains(PMSG::DONTWAIT);

        if is_nonblocking {
            // Non-blocking: just try once
            let len = self.try_recv(buffer)?;
            Ok((len, peer_endpoint))
        } else {
            // Blocking: wait until data is available
            loop {
                match self.try_recv(buffer) {
                    Ok(len) => return Ok((len, peer_endpoint)),
                    Err(SystemError::EAGAIN_OR_EWOULDBLOCK) => {
                        wq_wait_event_interruptible!(self.wait_queue, self.can_recv(), {})?;
                    }
                    Err(e) => return Err(e),
                }
            }
        }
    }

    fn recv_msg(&self, _msg: &mut MsgHdr, _flags: socket::PMSG) -> Result<usize, SystemError> {
        Err(SystemError::ENOSYS)
    }

    fn send(&self, buffer: &[u8], _flags: socket::PMSG) -> Result<usize, SystemError> {
        let result = self.try_send(buffer);

        // If send succeeded, notify peer's fasync_items for SIGIO
        if result.is_ok() && result.as_ref().unwrap() > &0 {
            if let Some(peer_weak) = self.peer.lock().as_ref() {
                if let Some(peer) = peer_weak.upgrade() {
                    peer.fasync_items.send_sigio();
                }
            }
        }

        result
    }

    fn send_msg(&self, _msg: &MsgHdr, _flags: socket::PMSG) -> Result<usize, SystemError> {
        todo!()
    }

    fn send_to(
        &self,
        _buffer: &[u8],
        _flags: socket::PMSG,
        _address: Endpoint,
    ) -> Result<usize, SystemError> {
        Err(SystemError::ENOSYS)
    }

    fn send_buffer_size(&self) -> usize {
        log::warn!("using default buffer size");
        UnixStreamSocket::DEFAULT_BUF_SIZE
    }

    fn recv_buffer_size(&self) -> usize {
        log::warn!("using default buffer size");
        UnixStreamSocket::DEFAULT_BUF_SIZE
    }

    fn epoll_items(&self) -> &EPollItems {
        &self.epitems
    }

    fn fasync_items(&self) -> &FAsyncItems {
        &self.fasync_items
    }

    fn option(&self, _level: PSOL, _name: usize, _value: &mut [u8]) -> Result<usize, SystemError> {
        todo!()
    }

    fn do_close(&self) -> Result<(), SystemError> {
        Ok(())
    }

    fn shutdown(&self, _how: common::ShutdownBit) -> Result<(), SystemError> {
        Err(SystemError::ENOSYS)
    }

    fn check_io_event(&self) -> crate::filesystem::epoll::EPollEventType {
        self.inner
            .read()
            .as_ref()
            .expect("UnixStreamSocket inner is None")
            .check_io_events()
    }

    fn socket_inode_id(&self) -> InodeId {
        self.inode_id
    }
}
