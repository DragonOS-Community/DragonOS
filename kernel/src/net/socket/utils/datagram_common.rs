use crate::filesystem::epoll::EPollEventType;
use crate::process::namespace::net_namespace::NetNamespace;
use crate::{libs::wait_queue::WaitQueue, net::socket::PMSG};
use alloc::fmt::Debug;
use alloc::sync::Arc;
use alloc::vec::Vec;
use system_error::SystemError;

//todo netlink和udp的操作相同，目前只是为netlink实现了下面的trait，后续为 UdpSocket实现下面的trait，提高复用性

pub trait Unbound {
    type Endpoint: Debug;
    type Bound;

    fn bind(
        &mut self,
        endpoint: &Self::Endpoint,
        wait_queue: Arc<WaitQueue>,
        netns: Arc<NetNamespace>,
    ) -> Result<Self::Bound, SystemError>;

    fn bind_ephemeral(
        &mut self,
        endpoint: &Self::Endpoint,
        wait_queue: Arc<WaitQueue>,
        netns: Arc<NetNamespace>,
    ) -> Result<Self::Bound, SystemError>;

    fn check_io_events(&self) -> EPollEventType;

    fn local_endpoint(&self) -> Option<Self::Endpoint>;
}

pub trait Bound {
    type Endpoint: Clone + Debug;

    fn bind(&mut self, _endpoint: &Self::Endpoint) -> Result<(), SystemError> {
        Err(SystemError::EINVAL)
    }

    fn local_endpoint(&self) -> Self::Endpoint;

    fn remote_endpoint(&self) -> Option<Self::Endpoint>;

    fn set_remote_endpoint(&mut self, endpoint: &Self::Endpoint);

    fn try_send(
        &self,
        buf: &[u8],
        to: &Self::Endpoint,
        flags: PMSG,
        destination_is_explicit: bool,
    ) -> Result<usize, SystemError>;

    fn try_send_vec(
        &self,
        buf: Vec<u8>,
        to: &Self::Endpoint,
        flags: PMSG,
        destination_is_explicit: bool,
    ) -> Result<usize, SystemError> {
        self.try_send(&buf, to, flags, destination_is_explicit)
    }

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
        netns: Arc<NetNamespace>,
    ) -> Result<(), SystemError> {
        let unbound = match self {
            Inner::Bound(bound) => return bound.bind(endpoint),
            Inner::Unbound(unbound) => unbound,
        };

        let bound = unbound.bind(endpoint, wait_queue, netns)?;
        *self = Inner::Bound(bound);

        // log::info!("Socket bound to endpoint: {:?}", endpoint);

        Ok(())
    }

    pub fn bind_ephemeral(
        &mut self,
        remote_endpoint: &UnboundSocket::Endpoint,
        wait_queue: Arc<WaitQueue>,
        netns: Arc<NetNamespace>,
    ) -> Result<(), SystemError> {
        let unbound_datagram = match self {
            Inner::Unbound(unbound) => unbound,
            Inner::Bound(_) => return Ok(()),
        };

        let bound = unbound_datagram.bind_ephemeral(remote_endpoint, wait_queue, netns)?;
        *self = Inner::Bound(bound);

        Ok(())
    }

    pub fn connect(
        &mut self,
        remote_endpoint: &UnboundSocket::Endpoint,
        wait_queue: Arc<WaitQueue>,
        netns: Arc<NetNamespace>,
    ) -> Result<(), SystemError> {
        self.bind_ephemeral(remote_endpoint, wait_queue, netns)?;

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
            Inner::Unbound(unbound) => unbound.local_endpoint(),
            Inner::Bound(bound) => Some(bound.local_endpoint()),
        }
    }

    pub fn peer_addr(&self) -> Option<UnboundSocket::Endpoint> {
        match self {
            Inner::Unbound(_) => None,
            Inner::Bound(bound) => bound.remote_endpoint(),
        }
    }

    // try_send 在下面:)
}
