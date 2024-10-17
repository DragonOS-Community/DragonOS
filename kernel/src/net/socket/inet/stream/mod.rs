use alloc::sync::{Arc, Weak};
use core::sync::atomic::{AtomicBool, AtomicUsize};
use system_error::SystemError::{self, *};

use crate::libs::rwlock::RwLock;
use crate::net::event_poll::EPollEventType;
use crate::net::net_core::poll_ifaces;
use crate::net::socket::*;
use crate::sched::SchedMode;
use inet::{InetSocket, UNSPECIFIED_LOCAL_ENDPOINT};
use smoltcp;

mod inner;
use inner::*;

mod option;
pub use option::Options as TcpOption;

type EP = EPollEventType;
#[derive(Debug)]
pub struct TcpSocket {
    inner: RwLock<Option<Inner>>,
    #[allow(dead_code)]
    shutdown: Shutdown, // TODO set shutdown status
    nonblock: AtomicBool,
    wait_queue: WaitQueue,
    self_ref: Weak<Self>,
    pollee: AtomicUsize,
}

impl TcpSocket {
    pub fn new(nonblock: bool) -> Arc<Self> {
        Arc::new_cyclic(|me| Self {
            inner: RwLock::new(Some(Inner::Init(Init::new()))),
            shutdown: Shutdown::new(),
            nonblock: AtomicBool::new(nonblock),
            wait_queue: WaitQueue::default(),
            self_ref: me.clone(),
            pollee: AtomicUsize::new((EP::EPOLLIN.bits() | EP::EPOLLOUT.bits()) as usize),
        })
    }

    pub fn new_established(inner: Established, nonblock: bool) -> Arc<Self> {
        Arc::new_cyclic(|me| Self {
            inner: RwLock::new(Some(Inner::Established(inner))),
            shutdown: Shutdown::new(),
            nonblock: AtomicBool::new(nonblock),
            wait_queue: WaitQueue::default(),
            self_ref: me.clone(),
            pollee: AtomicUsize::new((EP::EPOLLIN.bits() | EP::EPOLLOUT.bits()) as usize),
        })
    }

    pub fn is_nonblock(&self) -> bool {
        self.nonblock.load(core::sync::atomic::Ordering::Relaxed)
    }

    pub fn do_bind(&self, local_endpoint: smoltcp::wire::IpEndpoint) -> Result<(), SystemError> {
        let mut writer = self.inner.write();
        match writer.take().expect("Tcp Inner is None") {
            Inner::Init(inner) => {
                let bound = inner.bind(local_endpoint)?;
                if let Init::Bound((ref bound, _)) = bound {
                    bound
                        .iface()
                        .common()
                        .bind_socket(self.self_ref.upgrade().unwrap());
                }
                writer.replace(Inner::Init(bound));
                Ok(())
            }
            _ => Err(EINVAL),
        }
    }

    pub fn do_listen(&self, backlog: usize) -> Result<(), SystemError> {
        let mut writer = self.inner.write();
        let inner = writer.take().expect("Tcp Inner is None");
        let (listening, err) = match inner {
            Inner::Init(init) => {
                let listen_result = init.listen(backlog);
                match listen_result {
                    Ok(listening) => (Inner::Listening(listening), None),
                    Err((init, err)) => (Inner::Init(init), Some(err)),
                }
            }
            _ => (inner, Some(EINVAL)),
        };
        writer.replace(listening);
        drop(writer);

        if let Some(err) = err {
            return Err(err);
        }
        return Ok(());
    }

    pub fn try_accept(&self) -> Result<(Arc<TcpSocket>, smoltcp::wire::IpEndpoint), SystemError> {
        poll_ifaces();
        match self.inner.write().as_mut().expect("Tcp Inner is None") {
            Inner::Listening(listening) => listening.accept().map(|(stream, remote)| {
                (
                    TcpSocket::new_established(stream, self.is_nonblock()),
                    remote,
                )
            }),
            _ => Err(EINVAL),
        }
    }

    pub fn start_connect(
        &self,
        remote_endpoint: smoltcp::wire::IpEndpoint,
    ) -> Result<(), SystemError> {
        let mut writer = self.inner.write();
        let inner = writer.take().expect("Tcp Inner is None");
        let (init, err) = match inner {
            Inner::Init(init) => {
                let conn_result = init.connect(remote_endpoint);
                match conn_result {
                    Ok(connecting) => (
                        Inner::Connecting(connecting),
                        if self.is_nonblock() {
                            None
                        } else {
                            Some(EINPROGRESS)
                        },
                    ),
                    Err((init, err)) => (Inner::Init(init), Some(err)),
                }
            }
            Inner::Connecting(connecting) if self.is_nonblock() => {
                (Inner::Connecting(connecting), Some(EALREADY))
            }
            Inner::Connecting(connecting) => (Inner::Connecting(connecting), None),
            Inner::Listening(inner) => (Inner::Listening(inner), Some(EISCONN)),
            Inner::Established(inner) => (Inner::Established(inner), Some(EISCONN)),
        };
        writer.replace(init);

        drop(writer);

        poll_ifaces();

        if let Some(err) = err {
            return Err(err);
        }
        return Ok(());
    }

    pub fn finish_connect(&self) -> Result<(), SystemError> {
        let mut writer = self.inner.write();
        let Inner::Connecting(conn) = writer.take().expect("Tcp Inner is None") else {
            log::error!("TcpSocket::finish_connect: not Connecting");
            return Err(EINVAL);
        };

        let (inner, err) = conn.into_result();
        writer.replace(inner);
        drop(writer);

        if let Some(err) = err {
            return Err(err);
        }
        return Ok(());
    }

    pub fn check_connect(&self) -> Result<(), SystemError> {
        match self.inner.read().as_ref().expect("Tcp Inner is None") {
            Inner::Connecting(_) => Err(EAGAIN_OR_EWOULDBLOCK),
            Inner::Established(_) => Ok(()), // TODO check established
            _ => Err(EINVAL),                // TODO socket error options
        }
    }

    pub fn try_recv(&self, buf: &mut [u8]) -> Result<usize, SystemError> {
        self.inner
            .read()
            .as_ref()
            .map(|inner| {
                inner.iface().unwrap().poll();
                let result = match inner {
                    Inner::Established(inner) => inner.recv_slice(buf),
                    _ => Err(EINVAL),
                };
                inner.iface().unwrap().poll();
                result
            })
            .unwrap()
    }

    pub fn try_send(&self, buf: &[u8]) -> Result<usize, SystemError> {
        match self.inner.read().as_ref().expect("Tcp Inner is None") {
            Inner::Established(inner) => {
                let sent = inner.send_slice(buf);
                poll_ifaces();
                sent
            }
            _ => Err(EINVAL),
        }
    }

    fn update_events(&self) -> bool {
        match self.inner.read().as_ref().expect("Tcp Inner is None") {
            Inner::Init(_) => false,
            Inner::Connecting(connecting) => connecting.update_io_events(),
            Inner::Established(established) => {
                established.update_io_events(&self.pollee);
                false
            }
            Inner::Listening(listening) => {
                listening.update_io_events(&self.pollee);
                false
            }
        }
    }

    // should only call on accept
    fn is_acceptable(&self) -> bool {
        // (self.poll() & EP::EPOLLIN.bits() as usize) != 0
        self.inner.read().as_ref().unwrap().iface().unwrap().poll();
        EP::from_bits_truncate(self.poll() as u32).contains(EP::EPOLLIN)
    }
}

impl Socket for TcpSocket {
    fn wait_queue(&self) -> &WaitQueue {
        &self.wait_queue
    }

    fn get_name(&self) -> Result<Endpoint, SystemError> {
        match self.inner.read().as_ref().expect("Tcp Inner is None") {
            Inner::Init(Init::Unbound(_)) => Ok(Endpoint::Ip(UNSPECIFIED_LOCAL_ENDPOINT)),
            Inner::Init(Init::Bound((_, local))) => Ok(Endpoint::Ip(*local)),
            Inner::Connecting(connecting) => Ok(Endpoint::Ip(connecting.get_name())),
            Inner::Established(established) => Ok(Endpoint::Ip(established.local_endpoint())),
            Inner::Listening(listening) => Ok(Endpoint::Ip(listening.get_name())),
        }
    }

    fn bind(&self, endpoint: Endpoint) -> Result<(), SystemError> {
        if let Endpoint::Ip(addr) = endpoint {
            return self.do_bind(addr);
        }
        return Err(EINVAL);
    }

    fn connect(&self, endpoint: Endpoint) -> Result<(), SystemError> {
        if let Endpoint::Ip(addr) = endpoint {
            return self.start_connect(addr);
        }
        return Err(EINVAL);
    }

    fn poll(&self) -> usize {
        self.pollee.load(core::sync::atomic::Ordering::SeqCst)
    }

    fn listen(&self, backlog: usize) -> Result<(), SystemError> {
        self.do_listen(backlog)
    }

    fn accept(&self) -> Result<(Arc<Inode>, Endpoint), SystemError> {
        // could block io
        if self.is_nonblock() {
            self.try_accept()
        } else {
            loop {
                // log::debug!("TcpSocket::accept: wake up");
                match self.try_accept() {
                    Err(EAGAIN_OR_EWOULDBLOCK) => {
                        wq_wait_event_interruptible!(self.wait_queue, self.is_acceptable(), {})?;
                    }
                    result => break result,
                }
            }
        }
        .map(|(inner, endpoint)| (Inode::new(inner), Endpoint::Ip(endpoint)))
    }

    fn recv(&self, buffer: &mut [u8], _flags: PMSG) -> Result<usize, SystemError> {
        self.try_recv(buffer)
    }

    fn send(&self, buffer: &[u8], _flags: PMSG) -> Result<usize, SystemError> {
        self.try_send(buffer)
    }

    fn send_buffer_size(&self) -> usize {
        self.inner
            .read()
            .as_ref()
            .expect("Tcp Inner is None")
            .send_buffer_size()
    }

    fn recv_buffer_size(&self) -> usize {
        self.inner
            .read()
            .as_ref()
            .expect("Tcp Inner is None")
            .recv_buffer_size()
    }

    fn close(&self) -> Result<(), SystemError> {
        self.inner
            .read()
            .as_ref()
            .map(|inner| match inner {
                Inner::Connecting(_) => Err(EINPROGRESS),
                Inner::Established(es) => {
                    es.close();
                    es.release();
                    Ok(())
                }
                _ => Ok(()),
            })
            .unwrap_or(Ok(()))
    }
}

impl InetSocket for TcpSocket {
    fn on_iface_events(&self) {
        if self.update_events() {
            let _result = self.finish_connect();
            // set error
        }
    }
}
