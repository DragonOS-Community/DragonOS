use crate::{
    filesystem::epoll::EPollEventType,
    filesystem::vfs::fasync::FAsyncItems,
    libs::wait_queue::WaitQueue,
    net::socket::{
        common::EPollItems,
        netlink::{
            addr::{multicast::GroupIdSet, NetlinkSocketAddr},
            common::bound::BoundNetlink,
            receiver::{MessageQueue, MessageReceiver},
            table::SupportedNetlinkProtocol,
        },
        utils::datagram_common,
    },
    process::namespace::net_namespace::NetNamespace,
};
use alloc::sync::Arc;
use core::marker::PhantomData;
use system_error::SystemError;

#[derive(Debug)]
pub struct UnboundNetlink<P: SupportedNetlinkProtocol> {
    groups: GroupIdSet,
    epoll_items: Arc<EPollItems>,
    fasync_items: Arc<FAsyncItems>,
    phantom: PhantomData<BoundNetlink<P::Message>>,
}

impl<P: SupportedNetlinkProtocol> UnboundNetlink<P> {
    pub(super) fn new(epoll_items: Arc<EPollItems>, fasync_items: Arc<FAsyncItems>) -> Self {
        Self {
            groups: GroupIdSet::new_empty(),
            epoll_items,
            fasync_items,
            phantom: PhantomData,
        }
    }

    pub(super) fn addr(&self) -> NetlinkSocketAddr {
        NetlinkSocketAddr::new(0, self.groups)
    }

    pub(super) fn add_groups(&mut self, groups: GroupIdSet) {
        self.groups.add_groups(groups);
    }

    pub(super) fn drop_groups(&mut self, groups: GroupIdSet) {
        self.groups.drop_groups(groups);
    }
}

impl<P: SupportedNetlinkProtocol> datagram_common::Unbound for UnboundNetlink<P> {
    type Endpoint = NetlinkSocketAddr;
    type Bound = BoundNetlink<P::Message>;

    fn bind(
        &mut self,
        endpoint: &Self::Endpoint,
        wait_queue: Arc<WaitQueue>,
        netns: Arc<NetNamespace>,
    ) -> Result<BoundNetlink<P::Message>, SystemError> {
        let message_queue = MessageQueue::<P::Message>::new();
        let bound_handle = {
            let endpoint = {
                let mut endpoint = *endpoint;
                endpoint.add_groups(self.groups);
                endpoint
            };
            let receiver = MessageReceiver::new(
                message_queue.clone(),
                wait_queue,
                self.epoll_items.clone(),
                self.fasync_items.clone(),
            );
            <P as SupportedNetlinkProtocol>::bind(&endpoint, receiver, netns.clone())?
        };

        Ok(BoundNetlink::new(bound_handle, message_queue, netns))
    }

    fn bind_ephemeral(
        &mut self,
        _remote_endpoint: &Self::Endpoint,
        wait_queue: Arc<WaitQueue>,
        netns: Arc<NetNamespace>,
    ) -> Result<BoundNetlink<P::Message>, SystemError> {
        let message_queue = MessageQueue::<P::Message>::new();

        let bound_handle = {
            let endpoint = {
                let mut endpoint = NetlinkSocketAddr::new_unspecified();
                endpoint.add_groups(self.groups);
                endpoint
            };
            let receiver = MessageReceiver::new(
                message_queue.clone(),
                wait_queue,
                self.epoll_items.clone(),
                self.fasync_items.clone(),
            );
            <P as SupportedNetlinkProtocol>::bind(&endpoint, receiver, netns.clone())?
        };

        Ok(BoundNetlink::new(bound_handle, message_queue, netns))
    }

    fn check_io_events(&self) -> EPollEventType {
        EPollEventType::EPOLLOUT
    }

    fn local_endpoint(&self) -> Option<Self::Endpoint> {
        Some(self.addr())
    }
}
