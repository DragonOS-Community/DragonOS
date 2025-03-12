use core::sync::atomic::AtomicUsize;

use crate::libs::rwlock::RwLock;
use crate::net::socket::EPollEventType;
use crate::net::socket::{self, inet::Types};
use alloc::boxed::Box;
use alloc::vec::Vec;
use smoltcp;
use system_error::SystemError::{self, *};

// pub const DEFAULT_METADATA_BUF_SIZE: usize = 1024;
pub const DEFAULT_RX_BUF_SIZE: usize = 512 * 1024;
pub const DEFAULT_TX_BUF_SIZE: usize = 512 * 1024;

fn new_smoltcp_socket() -> smoltcp::socket::tcp::Socket<'static> {
    let rx_buffer = smoltcp::socket::tcp::SocketBuffer::new(vec![0; DEFAULT_RX_BUF_SIZE]);
    let tx_buffer = smoltcp::socket::tcp::SocketBuffer::new(vec![0; DEFAULT_TX_BUF_SIZE]);
    smoltcp::socket::tcp::Socket::new(rx_buffer, tx_buffer)
}

fn new_listen_smoltcp_socket<T>(local_endpoint: T) -> smoltcp::socket::tcp::Socket<'static>
where
    T: Into<smoltcp::wire::IpListenEndpoint>,
{
    let mut socket = new_smoltcp_socket();
    socket.listen(local_endpoint).unwrap();
    socket
}

#[derive(Debug)]
pub enum Init {
    Unbound(
        (
            Box<smoltcp::socket::tcp::Socket<'static>>,
            smoltcp::wire::IpVersion,
        ),
    ),
    Bound((socket::inet::BoundInner, smoltcp::wire::IpEndpoint)),
}

impl Init {
    pub(super) fn new(ver: smoltcp::wire::IpVersion) -> Self {
        Init::Unbound((Box::new(new_smoltcp_socket()), ver))
    }

    /// 传入一个已经绑定的socket
    pub(super) fn new_bound(inner: socket::inet::BoundInner) -> Self {
        let endpoint = inner.with::<smoltcp::socket::tcp::Socket, _, _>(|socket| {
            socket
                .local_endpoint()
                .expect("A Bound Socket Must Have A Local Endpoint")
        });
        Init::Bound((inner, endpoint))
    }

    pub(super) fn bind(
        self,
        local_endpoint: smoltcp::wire::IpEndpoint,
    ) -> Result<Self, SystemError> {
        match self {
            Init::Unbound((socket, _)) => {
                let bound = socket::inet::BoundInner::bind(*socket, &local_endpoint.addr)?;
                bound
                    .port_manager()
                    .bind_port(Types::Tcp, local_endpoint.port)?;
                // bound.iface().common().bind_socket()
                Ok(Init::Bound((bound, local_endpoint)))
            }
            Init::Bound(_) => {
                log::debug!("Already Bound");
                Err(EINVAL)
            }
        }
    }

    pub(super) fn bind_to_ephemeral(
        self,
        remote_endpoint: smoltcp::wire::IpEndpoint,
    ) -> Result<(socket::inet::BoundInner, smoltcp::wire::IpEndpoint), (Self, SystemError)> {
        match self {
            Init::Unbound((socket, ver)) => {
                let (bound, address) =
                    socket::inet::BoundInner::bind_ephemeral(*socket, remote_endpoint.addr)
                        .map_err(|err| (Self::new(ver), err))?;
                let bound_port = bound
                    .port_manager()
                    .bind_ephemeral_port(Types::Tcp)
                    .map_err(|err| (Self::new(ver), err))?;
                let endpoint = smoltcp::wire::IpEndpoint::new(address, bound_port);
                Ok((bound, endpoint))
            }
            Init::Bound(_) => Err((self, EINVAL)),
        }
    }

    pub(super) fn connect(
        self,
        remote_endpoint: smoltcp::wire::IpEndpoint,
    ) -> Result<Connecting, (Self, SystemError)> {
        let (inner, local) = match self {
            Init::Unbound(_) => self.bind_to_ephemeral(remote_endpoint)?,
            Init::Bound(inner) => inner,
        };
        if local.addr.is_unspecified() {
            return Err((Init::Bound((inner, local)), EINVAL));
        }
        let result = inner.with_mut::<smoltcp::socket::tcp::Socket, _, _>(|socket| {
            socket
                .connect(
                    inner.iface().smol_iface().lock().context(),
                    remote_endpoint,
                    local,
                )
                .map_err(|_| ECONNREFUSED)
        });
        match result {
            Ok(_) => Ok(Connecting::new(inner)),
            Err(err) => Err((Init::Bound((inner, local)), err)),
        }
    }

    /// # `listen`
    pub(super) fn listen(self, backlog: usize) -> Result<Listening, (Self, SystemError)> {
        let (inner, local) = match self {
            Init::Unbound(_) => {
                return Err((self, EINVAL));
            }
            Init::Bound(inner) => inner,
        };
        let listen_addr = if local.addr.is_unspecified() {
            smoltcp::wire::IpListenEndpoint::from(local.port)
        } else {
            smoltcp::wire::IpListenEndpoint::from(local)
        };
        log::debug!("listen at {:?}", listen_addr);
        let mut inners = Vec::new();
        if let Err(err) = || -> Result<(), SystemError> {
            for _ in 0..(backlog - 1) {
                // -1 because the first one is already bound
                let new_listen = socket::inet::BoundInner::bind(
                    new_listen_smoltcp_socket(listen_addr),
                    listen_addr
                        .addr
                        .as_ref()
                        .unwrap_or(&smoltcp::wire::IpAddress::from(
                            smoltcp::wire::Ipv4Address::UNSPECIFIED,
                        )),
                )?;
                inners.push(new_listen);
            }
            Ok(())
        }() {
            return Err((Init::Bound((inner, local)), err));
        }

        if let Err(err) = inner.with_mut::<smoltcp::socket::tcp::Socket, _, _>(|socket| {
            socket.listen(listen_addr).map_err(|_| ECONNREFUSED)
        }) {
            return Err((Init::Bound((inner, local)), err));
        }

        inners.push(inner);
        return Ok(Listening {
            inners,
            connect: AtomicUsize::new(0),
            listen_addr,
        });
    }

    pub(super) fn close(&self) {
        match self {
            Init::Unbound(_) => {}
            Init::Bound((inner, endpoint)) => {
                inner.port_manager().unbind_port(Types::Tcp, endpoint.port);
                inner.with_mut::<smoltcp::socket::tcp::Socket, _, _>(|socket| socket.close());
            }
        }
    }
}

#[derive(Debug, Default, Clone, Copy)]
enum ConnectResult {
    Connected,
    #[default]
    Connecting,
    Refused,
}

#[derive(Debug)]
pub struct Connecting {
    inner: socket::inet::BoundInner,
    result: RwLock<ConnectResult>,
}

impl Connecting {
    fn new(inner: socket::inet::BoundInner) -> Self {
        Connecting {
            inner,
            result: RwLock::new(ConnectResult::Connecting),
        }
    }

    pub fn with_mut<R, F: FnMut(&mut smoltcp::socket::tcp::Socket<'static>) -> R>(
        &self,
        f: F,
    ) -> R {
        self.inner.with_mut(f)
    }

    pub fn into_result(self) -> (Inner, Result<(), SystemError>) {
        use ConnectResult::*;
        let result = *self.result.read_irqsave();
        match result {
            Connecting => (Inner::Connecting(self), Err(EAGAIN_OR_EWOULDBLOCK)),
            Connected => (
                Inner::Established(Established { inner: self.inner }),
                Ok(()),
            ),
            Refused => (Inner::Init(Init::new_bound(self.inner)), Err(ECONNREFUSED)),
        }
    }

    pub unsafe fn into_established(self) -> Established {
        Established { inner: self.inner }
    }

    /// Returns `true` when `conn_result` becomes ready, which indicates that the caller should
    /// invoke the `into_result()` method as soon as possible.
    ///
    /// Since `into_result()` needs to be called only once, this method will return `true`
    /// _exactly_ once. The caller is responsible for not missing this event.
    #[must_use]
    pub(super) fn update_io_events(&self) -> bool {
        // if matches!(*self.result.read_irqsave(), ConnectResult::Connecting) {
        //     return false;
        // }

        self.inner
            .with_mut(|socket: &mut smoltcp::socket::tcp::Socket| {
                let mut result = self.result.write_irqsave();
                if matches!(*result, ConnectResult::Refused | ConnectResult::Connected) {
                    return false; // Already connected or refused
                }

                // Connected
                if socket.can_send() {
                    log::debug!("can send");
                    *result = ConnectResult::Connected;
                    return true;
                }
                // Connecting
                if socket.is_open() {
                    log::debug!("connecting");
                    *result = ConnectResult::Connecting;
                    return false;
                }
                // Refused
                *result = ConnectResult::Refused;
                return true;
            })
    }

    pub fn get_name(&self) -> smoltcp::wire::IpEndpoint {
        self.inner
            .with::<smoltcp::socket::tcp::Socket, _, _>(|socket| {
                socket
                    .local_endpoint()
                    .expect("A Connecting Tcp With No Local Endpoint")
            })
    }

    pub fn get_peer_name(&self) -> smoltcp::wire::IpEndpoint {
        self.inner
            .with::<smoltcp::socket::tcp::Socket, _, _>(|socket| {
                socket
                    .remote_endpoint()
                    .expect("A Connecting Tcp With No Remote Endpoint")
            })
    }
}

#[derive(Debug)]
pub struct Listening {
    inners: Vec<socket::inet::BoundInner>,
    connect: AtomicUsize,
    listen_addr: smoltcp::wire::IpListenEndpoint,
}

impl Listening {
    pub fn accept(&mut self) -> Result<(Established, smoltcp::wire::IpEndpoint), SystemError> {
        let connected: &mut socket::inet::BoundInner = self
            .inners
            .get_mut(self.connect.load(core::sync::atomic::Ordering::Relaxed))
            .unwrap();

        if connected.with::<smoltcp::socket::tcp::Socket, _, _>(|socket| !socket.is_active()) {
            return Err(EAGAIN_OR_EWOULDBLOCK);
        }

        let remote_endpoint = connected.with::<smoltcp::socket::tcp::Socket, _, _>(|socket| {
            socket
                .remote_endpoint()
                .expect("A Connected Tcp With No Remote Endpoint")
        });

        // log::debug!("local at {:?}", local_endpoint);

        let mut new_listen = socket::inet::BoundInner::bind(
            new_listen_smoltcp_socket(self.listen_addr),
            self.listen_addr
                .addr
                .as_ref()
                .unwrap_or(&smoltcp::wire::IpAddress::from(
                    smoltcp::wire::Ipv4Address::UNSPECIFIED,
                )),
        )?;

        // swap the connected socket with the new_listen socket
        // TODO is smoltcp socket swappable?
        core::mem::swap(&mut new_listen, connected);

        return Ok((Established { inner: new_listen }, remote_endpoint));
    }

    pub fn update_io_events(&self, pollee: &AtomicUsize) {
        let position = self.inners.iter().position(|inner| {
            inner.with::<smoltcp::socket::tcp::Socket, _, _>(|socket| socket.is_active())
        });

        if let Some(position) = position {
            self.connect
                .store(position, core::sync::atomic::Ordering::Relaxed);
            pollee.fetch_or(
                EPollEventType::EPOLLIN.bits() as usize,
                core::sync::atomic::Ordering::Relaxed,
            );
        } else {
            pollee.fetch_and(
                !EPollEventType::EPOLLIN.bits() as usize,
                core::sync::atomic::Ordering::Relaxed,
            );
        }
    }

    pub fn get_name(&self) -> smoltcp::wire::IpEndpoint {
        smoltcp::wire::IpEndpoint::new(
            self.listen_addr
                .addr
                .unwrap_or(smoltcp::wire::IpAddress::from(
                    smoltcp::wire::Ipv4Address::UNSPECIFIED,
                )),
            self.listen_addr.port,
        )
    }

    pub fn close(&self) {
        log::debug!("Close Listening Socket");
        let port = self.get_name().port;
        for inner in self.inners.iter() {
            inner.with_mut::<smoltcp::socket::tcp::Socket, _, _>(|socket| socket.close());
        }
        self.inners[0]
            .iface()
            .port_manager()
            .unbind_port(Types::Tcp, port);
    }

    pub fn release(&self) {
        // log::debug!("Release Listening Socket");
        for inner in self.inners.iter() {
            inner.release();
        }
    }
}

#[derive(Debug)]
pub struct Established {
    inner: socket::inet::BoundInner,
}

impl Established {
    pub fn with_mut<R, F: FnMut(&mut smoltcp::socket::tcp::Socket<'static>) -> R>(
        &self,
        f: F,
    ) -> R {
        self.inner.with_mut(f)
    }

    pub fn close(&self) {
        self.inner
            .with_mut::<smoltcp::socket::tcp::Socket, _, _>(|socket| socket.close());
        self.inner.iface().poll();
    }

    pub fn release(&self) {
        self.inner.release();
    }

    pub fn get_name(&self) -> smoltcp::wire::IpEndpoint {
        self.inner
            .with::<smoltcp::socket::tcp::Socket, _, _>(|socket| socket.local_endpoint())
            .unwrap()
    }

    pub fn get_peer_name(&self) -> smoltcp::wire::IpEndpoint {
        self.inner
            .with::<smoltcp::socket::tcp::Socket, _, _>(|socket| socket.remote_endpoint().unwrap())
    }

    pub fn recv_slice(&self, buf: &mut [u8]) -> Result<usize, SystemError> {
        self.inner
            .with_mut::<smoltcp::socket::tcp::Socket, _, _>(|socket| {
                use smoltcp::socket::tcp::RecvError::*;
                if socket.can_send() {
                    match socket.recv_slice(buf) {
                        Ok(size) => Ok(size),
                        Err(InvalidState) => {
                            log::error!("TcpSocket::try_recv: InvalidState");
                            Err(ENOTCONN)
                        }
                        Err(Finished) => Ok(0),
                    }
                } else {
                    Err(ENOBUFS)
                }
            })
    }

    pub fn send_slice(&self, buf: &[u8]) -> Result<usize, SystemError> {
        self.inner
            .with_mut::<smoltcp::socket::tcp::Socket, _, _>(|socket| {
                if socket.can_send() {
                    socket.send_slice(buf).map_err(|_| ECONNABORTED)
                } else {
                    Err(ENOBUFS)
                }
            })
    }

    pub fn update_io_events(&self, pollee: &AtomicUsize) {
        self.inner
            .with_mut::<smoltcp::socket::tcp::Socket, _, _>(|socket| {
                if socket.can_send() {
                    pollee.fetch_or(
                        EPollEventType::EPOLLOUT.bits() as usize,
                        core::sync::atomic::Ordering::Relaxed,
                    );
                } else {
                    pollee.fetch_and(
                        !EPollEventType::EPOLLOUT.bits() as usize,
                        core::sync::atomic::Ordering::Relaxed,
                    );
                }
                if socket.can_recv() {
                    pollee.fetch_or(
                        EPollEventType::EPOLLIN.bits() as usize,
                        core::sync::atomic::Ordering::Relaxed,
                    );
                } else {
                    pollee.fetch_and(
                        !EPollEventType::EPOLLIN.bits() as usize,
                        core::sync::atomic::Ordering::Relaxed,
                    );
                }
            })
    }
}

#[derive(Debug)]
pub enum Inner {
    Init(Init),
    Connecting(Connecting),
    Listening(Listening),
    Established(Established),
}

impl Inner {
    pub fn send_buffer_size(&self) -> usize {
        match self {
            Inner::Init(_) => DEFAULT_TX_BUF_SIZE,
            Inner::Connecting(conn) => conn.with_mut(|socket| socket.send_capacity()),
            // only the first socket in the list is used for sending
            Inner::Listening(listen) => listen.inners[0]
                .with_mut::<smoltcp::socket::tcp::Socket, _, _>(|socket| socket.send_capacity()),
            Inner::Established(est) => est.with_mut(|socket| socket.send_capacity()),
        }
    }

    pub fn recv_buffer_size(&self) -> usize {
        match self {
            Inner::Init(_) => DEFAULT_RX_BUF_SIZE,
            Inner::Connecting(conn) => conn.with_mut(|socket| socket.recv_capacity()),
            // only the first socket in the list is used for receiving
            Inner::Listening(listen) => listen.inners[0]
                .with_mut::<smoltcp::socket::tcp::Socket, _, _>(|socket| socket.recv_capacity()),
            Inner::Established(est) => est.with_mut(|socket| socket.recv_capacity()),
        }
    }

    pub fn iface(&self) -> Option<&alloc::sync::Arc<dyn crate::driver::net::Iface>> {
        match self {
            Inner::Init(_) => None,
            Inner::Connecting(conn) => Some(conn.inner.iface()),
            Inner::Listening(listen) => Some(listen.inners[0].iface()),
            Inner::Established(est) => Some(est.inner.iface()),
        }
    }
}
