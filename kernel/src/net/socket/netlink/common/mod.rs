use crate::{
    libs::{rwlock::RwLock, wait_queue::WaitQueue},
    net::socket::{
        endpoint::Endpoint,
        netlink::{
            addr::{multicast::GroupIdSet, NetlinkSocketAddr},
            common::{bound::BoundNetlink, unbound::UnboundNetlink},
            table::SupportedNetlinkProtocol,
        },
        utils::datagram_common::{select_remote_and_bind, Bound, Inner},
        Socket,
    },
};
use alloc::sync::Arc;
use core::sync::atomic::AtomicBool;
use system_error::SystemError;

pub(super) mod bound;
mod unbound;

#[derive(Debug)]
pub struct NetlinkSocket<P: SupportedNetlinkProtocol> {
    inner: RwLock<Inner<UnboundNetlink<P>, BoundNetlink<P::Message>>>,

    is_nonblocking: AtomicBool,
    wait_queue: Arc<WaitQueue>,
}

impl<P: SupportedNetlinkProtocol> NetlinkSocket<P>
where
    BoundNetlink<P::Message>: Bound<Endpoint = NetlinkSocketAddr>,
{
    pub fn new(is_nonblocking: bool) -> Arc<Self> {
        let unbound = UnboundNetlink::new();
        Arc::new(Self {
            inner: RwLock::new(Inner::Unbound(unbound)),
            is_nonblocking: AtomicBool::new(is_nonblocking),
            wait_queue: Arc::new(WaitQueue::default()),
        })
    }

    fn try_send(
        &self,
        buf: &[u8],
        to: Option<NetlinkSocketAddr>,
        flags: crate::net::socket::PMSG,
    ) -> Result<usize, SystemError> {
        let send_bytes = select_remote_and_bind(
            &self.inner,
            to,
            || {
                self.inner.write().bind_ephemeral(
                    &NetlinkSocketAddr::new_unspecified(),
                    self.wait_queue.clone(),
                )
            },
            |bound, remote| bound.try_send(buf, &remote, flags),
        )?;
        // todo pollee invalidate??

        Ok(send_bytes)
    }

    fn try_recv(
        &self,
        buf: &mut [u8],
        flags: crate::net::socket::PMSG,
    ) -> Result<(usize, Endpoint), SystemError> {
        let (recv_bytes, endpoint) = self
            .inner
            .read()
            .try_recv(buf, flags)
            .map(|(recv_bytes, remote_endpoint)| (recv_bytes, remote_endpoint.into()))?;
        // todo self.pollee.invalidate();

        Ok((recv_bytes, endpoint))
    }
}

impl<P: SupportedNetlinkProtocol + 'static> Socket for NetlinkSocket<P>
where
    BoundNetlink<P::Message>: Bound<Endpoint = NetlinkSocketAddr>,
{
    fn connect(
        &self,
        endpoint: crate::net::socket::endpoint::Endpoint,
    ) -> Result<(), system_error::SystemError> {
        let endpoint = endpoint.try_into()?;

        self.inner
            .write()
            .connect(&endpoint, self.wait_queue.clone())
    }

    fn bind(
        &self,
        endpoint: crate::net::socket::endpoint::Endpoint,
    ) -> Result<(), system_error::SystemError> {
        let endpoint = endpoint.try_into()?;

        self.inner.write().bind(&endpoint, self.wait_queue.clone())
    }

    fn send_to(
        &self,
        buffer: &[u8],
        flags: crate::net::socket::PMSG,
        address: crate::net::socket::endpoint::Endpoint,
    ) -> Result<usize, system_error::SystemError> {
        let endpoint = address.try_into()?;

        self.try_send(buffer, Some(endpoint), flags)
    }

    fn wait_queue(&self) -> &WaitQueue {
        &self.wait_queue
    }

    fn recv_from(
        &self,
        buffer: &mut [u8],
        flags: crate::net::socket::PMSG,
        address: Option<crate::net::socket::endpoint::Endpoint>,
    ) -> Result<(usize, crate::net::socket::endpoint::Endpoint), system_error::SystemError> {
        //todo 处理一下阻塞的逻辑

        self.try_recv(buffer, flags)
    }

    fn poll(&self) -> usize {
        todo!()
    }

    fn send_buffer_size(&self) -> usize {
        log::warn!("send_buffer_size is implemented to 0");
        0
    }

    fn recv_buffer_size(&self) -> usize {
        log::warn!("recv_buffer_size is implemented to 0");
        0
    }
}

impl<P: SupportedNetlinkProtocol> NetlinkSocket<P> {
    pub fn is_nonblocking(&self) -> bool {
        self.is_nonblocking
            .load(core::sync::atomic::Ordering::Relaxed)
    }

    pub fn set_nonblocking(&self, nonblocking: bool) {
        self.is_nonblocking
            .store(nonblocking, core::sync::atomic::Ordering::Relaxed);
    }
}

impl<P: SupportedNetlinkProtocol> Inner<UnboundNetlink<P>, BoundNetlink<P::Message>> {
    fn add_groups(&mut self, groups: GroupIdSet) {
        match self {
            Inner::Bound(bound) => bound.add_groups(groups),
            Inner::Unbound(unbound) => unbound.add_groups(groups),
        }
    }

    fn drop_groups(&mut self, groups: GroupIdSet) {
        match self {
            Inner::Unbound(unbound) => unbound.drop_groups(groups),
            Inner::Bound(bound) => bound.drop_groups(groups),
        }
    }
}
