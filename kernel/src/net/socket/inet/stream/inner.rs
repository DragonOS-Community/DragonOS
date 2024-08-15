use system_error::SystemError::{self, *};
use crate::net::socket;
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
pub struct Unbound {
    socket: smoltcp::socket::tcp::Socket<'static>,
}

impl Unbound {
    pub fn new() -> Self {
        Self { socket: new_smoltcp_socket() }
    }

    pub fn bind(self, local_endpoint: smoltcp::wire::IpEndpoint) -> Result<Connecting, SystemError> {
        Ok( Connecting {
            inner: socket::inet::BoundInner::bind(
                self.socket, 
                &local_endpoint.addr,
            )?,
            local_endpoint,
        })
    }

    pub fn close(&mut self) {
        self.socket.close();
    }
}

#[derive(Debug)]
pub struct Connecting {
    inner: socket::inet::BoundInner,
    local_endpoint: smoltcp::wire::IpEndpoint,
}

impl Connecting {
    pub fn with_mut<R, F: FnMut(&mut smoltcp::socket::tcp::Socket<'static>) -> R>(&self, f: F) -> R {
        self.inner.with_mut(f)
    }

    pub fn listen(self, backlog: usize) -> Result<Listening, SystemError> {
        let mut inners = Vec::new();
        self.with_mut(|socket| {
            socket.listen(self.local_endpoint).map_err(|_| EADDRNOTAVAIL)
        })?;
        inners.push(self.inner);
        for _ in 1..backlog {
            inners.push(socket::inet::BoundInner::bind(
                new_listen_smoltcp_socket(self.local_endpoint), 
                &self.local_endpoint.addr,
            )?);
        }
        return Ok( Listening {inners} );
    }

    pub fn connect(self, remote_endpoint: smoltcp::wire::IpEndpoint) -> Result<Established, SystemError> {
        self.inner.with_mut::<smoltcp::socket::tcp::Socket, _, _>(|socket| {
            socket.connect(
                self.inner.iface().smol_iface().lock().context(),
                remote_endpoint,
                self.local_endpoint
            ).map_err(|_| ECONNREFUSED)
        })?;
        return Ok( Established { inner: self.inner });
    }

    pub fn close(self) {
        self.inner.with_mut::<smoltcp::socket::tcp::Socket, _, _>(|socket| {
            socket.close();
        });
    }

    pub fn local_endpoint(&self) -> &smoltcp::wire::IpEndpoint {
        &self.local_endpoint
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

    pub fn recv_slice(&self, buf: &mut [u8]) -> Result<usize, smoltcp::socket::tcp::RecvError> {
        self.inner.with_mut::<smoltcp::socket::tcp::Socket, _, _>(|socket| {
            socket.recv_slice(buf)
        })
    }

    pub fn send_slice(&self, buf: &[u8]) -> Result<usize, SystemError> {
        self.inner.with_mut::<smoltcp::socket::tcp::Socket, _, _>(|socket| {
            socket.send_slice(buf).map_err(|_| ECONNABORTED)
        })
    }
}

#[derive(Debug)]
pub enum Inner {
    Unbound(Unbound),
    Connecting(Connecting),
    Listening(Listening),
    Established(Established),
}