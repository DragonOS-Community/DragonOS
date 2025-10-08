use crate::{
    libs::rwlock::RwLock,
    net::socket::{self, *},
};
use crate::{
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
use alloc::sync::Arc;
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
    wait_queue: Arc<WaitQueue>,

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
            is_nonblocking: AtomicBool::new(is_nonblocking),
            is_seqpacket,
            epitems: EPollItems::default(),
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
            is_nonblocking: AtomicBool::new(is_nonblocking),
            is_seqpacket,
            epitems: EPollItems::default(),
        })
    }

    pub fn new(is_nonblocking: bool, is_seqpacket: bool) -> Arc<Self> {
        Self::new_init(Init::new(), is_nonblocking, is_seqpacket)
    }

    pub fn new_pair(is_nonblocking: bool, is_seqpacket: bool) -> (Arc<Self>, Arc<Self>) {
        let (conn_a, conn_b) = Connected::new_pair(None, None);
        (
            Self::new_connected(conn_a, is_nonblocking, is_seqpacket),
            Self::new_connected(conn_b, is_nonblocking, is_seqpacket),
        )
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
        use crate::sched::SchedMode;

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

    fn recv(&self, buffer: &mut [u8], _flags: socket::PMSG) -> Result<usize, SystemError> {
        self.try_recv(buffer)
    }

    fn recv_from(
        &self,
        _buffer: &mut [u8],
        _flags: socket::PMSG,
        _address: Option<Endpoint>,
    ) -> Result<(usize, Endpoint), SystemError> {
        todo!()

        // if flags.contains(PMSG::OOB) {
        //     return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
        // }
        // if !flags.contains(PMSG::DONTWAIT) {
        //     loop {
        //         log::debug!("socket try recv from");

        //         wq_wait_event_interruptible!(
        //             self.wait_queue,
        //             self.can_recv()? || self.is_peer_shutdown()?,
        //             {}
        //         )?;
        //         // connect锁和flag判断顺序不正确，应该先判断在
        //         log::debug!("try recv");

        //         match &*self.inner.write() {
        //             Inner::Connected(connected) => match connected.try_recv(buffer) {
        //                 Ok(usize) => {
        //                     log::debug!("recvs from successfully");
        //                     return Ok((usize, connected.peer_endpoint().unwrap().clone()));
        //                 }
        //                 Err(_) => continue,
        //             },
        //             _ => {
        //                 log::error!("the socket is not connected");
        //                 return Err(SystemError::ENOTCONN);
        //             }
        //         }
        //     }
        // } else {
        //     unimplemented!("unimplemented non_block")
        // }
    }

    fn recv_msg(&self, _msg: &mut MsgHdr, _flags: socket::PMSG) -> Result<usize, SystemError> {
        Err(SystemError::ENOSYS)
    }

    fn send(&self, buffer: &[u8], _flags: socket::PMSG) -> Result<usize, SystemError> {
        self.try_send(buffer)
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
}
