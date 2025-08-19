use crate::filesystem::epoll::EPollEventType;
use crate::{
    libs::{rwlock::RwLock, wait_queue::WaitQueue},
    net::socket::PMSG,
};
use alloc::sync::Arc;
use core::panic;
use system_error::SystemError;

//todo netlink和udp的操作相同，目前只是为netlink实现了下面的trait，后续为 UdpSocket实现下面的trait，提高复用性

pub trait Unbound {
    type Endpoint;
    type Bound;

    fn bind(
        &mut self,
        endpoint: &Self::Endpoint,
        wait_queue: Arc<WaitQueue>,
    ) -> Result<Self::Bound, SystemError>;

    fn bind_ephemeral(
        &mut self,
        endpoint: &Self::Endpoint,
        wait_queue: Arc<WaitQueue>,
    ) -> Result<Self::Bound, SystemError>;

    fn check_io_events(&self) -> EPollEventType;
}

pub trait Bound {
    type Endpoint: Clone;

    fn bind(&mut self, _endpoint: &Self::Endpoint) -> Result<(), SystemError> {
        Err(SystemError::EINVAL)
    }

    fn local_endpoint(&self) -> Self::Endpoint;

    fn remote_endpoint(&self) -> Option<Self::Endpoint>;

    fn set_remote_endpoint(&mut self, endpoint: &Self::Endpoint);

    fn try_recv(
        &self,
        writer: &mut [u8],
        flags: PMSG,
    ) -> Result<(usize, Self::Endpoint), SystemError>;

    fn try_send(&self, buf: &[u8], to: &Self::Endpoint, flags: PMSG) -> Result<usize, SystemError>;

    fn check_io_events(&self) -> EPollEventType;
}

#[derive(Debug)]
pub enum Inner<UnboundSocket, BoundSocket> {
    Unbound(UnboundSocket),
    Bound(BoundSocket),
}

impl<UnboundSocket, BoundSocket> Inner<UnboundSocket, BoundSocket>
where
    UnboundSocket: Unbound<Endpoint = BoundSocket::Endpoint, Bound = BoundSocket>,
    BoundSocket: Bound,
{
    pub fn bind(
        &mut self,
        endpoint: &UnboundSocket::Endpoint,
        wait_queue: Arc<WaitQueue>,
    ) -> Result<(), SystemError> {
        let unbound = match self {
            Inner::Bound(bound) => return bound.bind(endpoint),
            Inner::Unbound(unbound) => unbound,
        };

        let bound = unbound.bind(endpoint, wait_queue)?;
        *self = Inner::Bound(bound);

        Ok(())
    }

    pub fn bind_ephemeral(
        &mut self,
        remote_endpoint: &UnboundSocket::Endpoint,
        wait_queue: Arc<WaitQueue>,
    ) -> Result<(), SystemError> {
        let unbound_datagram = match self {
            Inner::Unbound(unbound) => unbound,
            Inner::Bound(_) => return Ok(()),
        };

        let bound = unbound_datagram.bind_ephemeral(remote_endpoint, wait_queue)?;
        *self = Inner::Bound(bound);

        Ok(())
    }

    pub fn connect(
        &mut self,
        remote_endpoint: &UnboundSocket::Endpoint,
        wait_queue: Arc<WaitQueue>,
    ) -> Result<(), SystemError> {
        self.bind_ephemeral(remote_endpoint, wait_queue)?;

        let bound = match self {
            Inner::Unbound(_) => {
                unreachable!(
                    "`bind_to_ephemeral_endpoint` succeeds so the socket cannot be unbound"
                );
            }
            Inner::Bound(bound_datagram) => bound_datagram,
        };
        bound.set_remote_endpoint(remote_endpoint);

        Ok(())
    }

    pub fn check_io_events(&self) -> EPollEventType {
        match self {
            Inner::Unbound(unbound_datagram) => unbound_datagram.check_io_events(),
            Inner::Bound(bound_datagram) => bound_datagram.check_io_events(),
        }
    }

    pub fn addr(&self) -> Option<UnboundSocket::Endpoint> {
        match self {
            Inner::Unbound(_) => None,
            Inner::Bound(bound) => bound.remote_endpoint(),
        }
    }

    pub fn peer_addr(&self) -> Option<UnboundSocket::Endpoint> {
        match self {
            Inner::Unbound(_) => None,
            Inner::Bound(bound) => bound.remote_endpoint(),
        }
    }

    pub fn try_recv(
        &self,
        writer: &mut [u8],
        flags: PMSG,
    ) -> Result<(usize, UnboundSocket::Endpoint), SystemError> {
        match self {
            Inner::Unbound(_) => Err(SystemError::EAGAIN_OR_EWOULDBLOCK),
            Inner::Bound(bound) => bound.try_recv(writer, flags),
        }
    }

    // try_send 在下面:)
}

pub fn select_remote_and_bind<UnboundSocket, BoundSocket, B, F, R>(
    inner_lock: &RwLock<Inner<UnboundSocket, BoundSocket>>,
    remote: Option<UnboundSocket::Endpoint>,
    bind_ephemeral: B,
    op: F,
) -> Result<R, SystemError>
where
    UnboundSocket: Unbound<Endpoint = BoundSocket::Endpoint, Bound = BoundSocket>,
    BoundSocket: Bound,
    B: FnOnce() -> Result<(), SystemError>,
    F: FnOnce(&BoundSocket, UnboundSocket::Endpoint) -> Result<R, SystemError>,
{
    let mut inner = inner_lock.read();

    // 这里用 loop 只是为了用 break :)
    #[expect(clippy::never_loop)]
    let bound = loop {
        if let Inner::Bound(bound) = &*inner {
            break bound;
        }

        drop(inner);
        bind_ephemeral()?;

        inner = inner_lock.read();

        if let Inner::Bound(bound_datagram) = &*inner {
            break bound_datagram;
        }

        panic!("");
    };

    let remote_endpoint = match remote {
        Some(r) => r.clone(),
        None => bound.remote_endpoint().ok_or(SystemError::EDESTADDRREQ)?,
    };

    op(bound, remote_endpoint)
}
