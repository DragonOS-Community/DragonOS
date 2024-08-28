use system_error::SystemError::{self, *};
use crate::net::socket::{self, inet::Types};
use crate::libs::rwlock::RwLock;
use alloc::vec::Vec;
use smoltcp;

pub const DEFAULT_METADATA_BUF_SIZE: usize = 1024;
pub const DEFAULT_RX_BUF_SIZE: usize = 512 * 1024;
pub const DEFAULT_TX_BUF_SIZE: usize = 512 * 1024;

fn new_smoltcp_socket() -> smoltcp::socket::tcp::Socket<'static> {
    let rx_buffer = smoltcp::socket::tcp::SocketBuffer::new(
        vec![0; DEFAULT_RX_BUF_SIZE]
    );
    let tx_buffer = smoltcp::socket::tcp::SocketBuffer::new(
        vec![0; DEFAULT_TX_BUF_SIZE]
    );
    smoltcp::socket::tcp::Socket::new(rx_buffer, tx_buffer)
}

fn new_listen_smoltcp_socket(local_endpoint: smoltcp::wire::IpEndpoint) -> smoltcp::socket::tcp::Socket<'static> {
    let mut socket = new_smoltcp_socket();
    socket.listen(local_endpoint).unwrap();
    socket
}

#[derive(Debug)]
pub enum Init {
    Unbound(smoltcp::socket::tcp::Socket<'static>),
    Bound((socket::inet::BoundInner, smoltcp::wire::IpEndpoint)),
}

impl Init {
    pub(super) fn new() -> Self {
        Init::Unbound(new_smoltcp_socket())
    }

    /// 传入一个已经绑定的socket
    pub(super) fn new_bound(inner: socket::inet::BoundInner) -> Self {
        let endpoint = inner.with::<smoltcp::socket::tcp::Socket, _, _>(|socket| {
            socket.local_endpoint().expect("A Bound Socket Must Have A Local Endpoint")
        });
        Init::Bound((inner, endpoint))
    }

    pub(super) fn bind(self, local_endpoint: smoltcp::wire::IpEndpoint) -> Result<Self, SystemError> {
        match self {
            Init::Unbound(socket) => {
                let bound = socket::inet::BoundInner::bind(
                    socket, 
                    &local_endpoint.addr,
                )?;
                bound.port_manager().bind_port(Types::Tcp, local_endpoint.port)?;
                // bound.iface().common().bind_socket()
                Ok( Init::Bound((bound, local_endpoint)) )
            },
            Init::Bound(_) => {
                Err(EINVAL)
            }
        }
    }

    pub(super) fn bind_to_ephemeral(self, remote_endpoint: smoltcp::wire::IpEndpoint) 
        -> Result<(socket::inet::BoundInner, smoltcp::wire::IpEndpoint), (Self, SystemError)> 
    {
        match self {
            Init::Unbound(socket) => {
                let (bound, address) = socket::inet::BoundInner::bind_ephemeral(
                    socket, 
                    remote_endpoint.addr,
                ).map_err(|err| (Self::new(), err))?;
                let bound_port = bound
                    .port_manager()
                    .bind_ephemeral_port(Types::Tcp)
                    .map_err(|err| (Self::new(), err))?;
                let endpoint = smoltcp::wire::IpEndpoint::new(address, bound_port);
                Ok( (bound, endpoint) )
            },
            Init::Bound(_) => {
                Err((self, EINVAL))
            }
        }
    }

    pub(super) fn connect(self, remote_endpoint: smoltcp::wire::IpEndpoint) -> Result<Connecting, (Self, SystemError)> {
        let (inner, local) = match self {
            Init::Unbound(_) => {
                self.bind_to_ephemeral(remote_endpoint)?
            },
            Init::Bound(inner) => inner,
        };
        let result = inner.with_mut::<smoltcp::socket::tcp::Socket, _, _>(|socket| {
            socket.connect(
                inner.iface().smol_iface().lock().context(),
                remote_endpoint,
                local
            ).map_err(|_| ECONNREFUSED)
        });
        match result {
            Ok(_) => {
                Ok( Connecting::new(inner) )
            },
            Err(err) => {
                Err((Init::Bound((inner, local)), err))
            }
        }
    }

    /// # `listen`
    pub(super) fn listen(self, backlog: usize) -> Result<Listening, (Self, SystemError)> {
        let (inner, local) = match self {
            Init::Unbound(_) => {
                return Err((self, EINVAL));
            },
            Init::Bound(inner) => inner,
        };
        let mut inners = Vec::new();
        if let Err(err) = || -> Result<(), SystemError> {
            for _ in 0..(backlog-1) { // -1 because the first one is already bound
                let new_listen = socket::inet::BoundInner::bind(
                    new_listen_smoltcp_socket(local), 
                    &local.addr,
                )?;
                inners.push(new_listen);
            }
            Ok(())
        }() { 
            return Err((Init::Bound((inner, local)), err));
        }

        if let Err(err) = inner.with_mut::<smoltcp::socket::tcp::Socket, _, _>(|socket| {
            socket.listen(local).map_err(|_| ECONNREFUSED)
        }) {
            return Err((Init::Bound((inner, local)), err));
        }

        inners.push(inner);
        return Ok( Listening { inners });
    }
}

#[derive(Debug, Default, Clone, Copy)]
enum ConnectResult {
    Connected,
    #[default] Connecting,
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

    pub fn with_mut<R, F: FnMut(&mut smoltcp::socket::tcp::Socket<'static>) -> R>(&self, f: F) -> R {
        self.inner.with_mut(f)
    }

    pub fn into_result(self) -> (Inner, Option<SystemError>) {
        use ConnectResult::*;
        let result = self.result.read_irqsave().clone();
        match result {
            Connecting => (Inner::Connecting(self), Some(EAGAIN_OR_EWOULDBLOCK)),
            Connected => (Inner::Established(Established { inner: self.inner }), None),
            Refused => (Inner::Init(Init::new_bound(self.inner)), Some(ECONNREFUSED)),
        }
    }

    /// Returns `true` when `conn_result` becomes ready, which indicates that the caller should
    /// invoke the `into_result()` method as soon as possible.
    ///
    /// Since `into_result()` needs to be called only once, this method will return `true`
    /// _exactly_ once. The caller is responsible for not missing this event.
    #[must_use]
    pub(super) fn update_io_events(&self) -> bool {
        if matches!(*self.result.read_irqsave(), ConnectResult::Connecting) {
            return false;
        }

        self.inner.with_mut(|socket: &mut smoltcp::socket::tcp::Socket| {
            let mut result = self.result.write_irqsave();
            if matches!(*result, ConnectResult::Refused | ConnectResult::Connected) {
                return false; // Already connected or refused
            }

            // Connected
            if socket.can_send() {
                *result = ConnectResult::Connected;
                return true;
            }
            // Connecting
            if socket.is_open() {
                return false;
            }
            // Refused
            *result = ConnectResult::Refused;
            return true;
        })
    }
}

#[derive(Debug)]
pub struct Listening {
    inners: Vec<socket::inet::BoundInner>,
}

impl Listening {
    pub fn accept(&mut self) -> Result<(Established, smoltcp::wire::IpEndpoint), SystemError> {
        let local_endpoint = self.inners[0].with::<smoltcp::socket::tcp::Socket, _, _>(|socket| {
            socket.local_endpoint()
        }).ok_or_else(||{
            log::error!("A Listening Tcp With No Local Endpoint");
            EINVAL
        })?;

        let mut new_listen = socket::inet::BoundInner::bind(
            new_listen_smoltcp_socket(local_endpoint), 
            &local_endpoint.addr,
        )?;

        let connected: &mut socket::inet::BoundInner = self.inners.iter_mut().find(|inner| {
            inner.with::<smoltcp::socket::tcp::Socket, _, _>(|socket| {
                socket.is_active()
            })
        }).ok_or(EAGAIN_OR_EWOULDBLOCK)?;

        // swap the connected socket with the new_listen socket
        // TODO is smoltcp socket swappable?
        core::mem::swap(&mut new_listen, connected);

        let remote_endpoint = connected.with::<smoltcp::socket::tcp::Socket, _, _>(|socket| {
            // haven't check ECONNABORTED is the right error
            socket.remote_endpoint().ok_or(ECONNABORTED)
        })?;

        return Ok (( Established { inner: new_listen }, remote_endpoint));
    }
}

#[derive(Debug)]
pub struct Established {
    inner: socket::inet::BoundInner,
}

impl Established {
    pub fn with_mut<R, F: FnMut(&mut smoltcp::socket::tcp::Socket<'static>) -> R>(&self, f: F) -> R {
        self.inner.with_mut(f)
    }

    pub fn with<R, F: Fn(&smoltcp::socket::tcp::Socket<'static>) -> R>(&self, f: F) -> R {
        self.inner.with(f)
    }

    pub fn close(self) {
        self.inner.with_mut::<smoltcp::socket::tcp::Socket, _, _>(|socket| {
            socket.close();
        });
        self.inner.release();
    }

    pub fn local_endpoint(&self) -> smoltcp::wire::IpEndpoint {
        self.inner.with::<smoltcp::socket::tcp::Socket, _, _>(|socket| {
            socket.local_endpoint()
        }).unwrap()
    }

    pub fn remote_endpoint(&self) -> smoltcp::wire::IpEndpoint {
        self.inner.with::<smoltcp::socket::tcp::Socket, _, _>(|socket| {
            socket.remote_endpoint().unwrap()
        })
    }

    pub fn recv_slice(&self, buf: &mut [u8]) -> Result<usize, SystemError> {
        self.inner.with_mut::<smoltcp::socket::tcp::Socket, _, _>(|socket| {
            use smoltcp::socket::tcp::RecvError::*;
            if socket.can_send() {
                match socket.recv_slice(buf) {
                    Ok(size) => Ok(size),
                    Err(InvalidState) => {
                        log::error!("TcpSocket::try_recv: InvalidState");
                        Err(ENOTCONN)
                    },
                    Err(Finished) => {
                        Ok(0)
                    }
                }
            } else {
                Err(ENOBUFS)
            }
        })
    }

    pub fn send_slice(&self, buf: &[u8]) -> Result<usize, SystemError> {
        self.inner.with_mut::<smoltcp::socket::tcp::Socket, _, _>(|socket| {
            if socket.can_send() {
                socket.send_slice(buf).map_err(|_| ECONNABORTED)
            } else {
                Err(ENOBUFS)
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