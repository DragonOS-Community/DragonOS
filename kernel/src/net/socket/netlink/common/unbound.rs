use crate::{
    filesystem::epoll::EPollEventType,
    libs::wait_queue::WaitQueue,
    net::socket::{
        netlink::{
            addr::{multicast::GroupIdSet, NetlinkSocketAddr},
            common::bound::BoundNetlink,
            receiver::{MessageQueue, MessageReceiver},
            table::SupportedNetlinkProtocol,
        },
        utils::datagram_common,
    },
};
use alloc::sync::Arc;
use core::marker::PhantomData;
use system_error::SystemError;

#[derive(Debug)]
pub struct UnboundNetlink<P: SupportedNetlinkProtocol> {
    groups: GroupIdSet,
    phantom: PhantomData<BoundNetlink<P::Message>>,
}

impl<P: SupportedNetlinkProtocol> UnboundNetlink<P> {
    pub(super) fn new() -> Self {
        Self {
            groups: GroupIdSet::new_empty(),
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
        endpoint: &NetlinkSocketAddr,
        wait_queue: Arc<WaitQueue>,
    ) -> Result<BoundNetlink<P::Message>, SystemError> {
        let message_queue = MessageQueue::<P::Message>::new();
        let bound_handle = {
            let endpoint = {
                let mut endpoint = *endpoint;
                endpoint.add_groups(self.groups);
                endpoint
            };
            let receiver = MessageReceiver::new(message_queue.clone(), wait_queue);
            <P as SupportedNetlinkProtocol>::bind(&endpoint, receiver)?
        };

        Ok(BoundNetlink::new(bound_handle, message_queue))
    }

    fn bind_ephemeral(
        &mut self,
        _remote_endpoint: &Self::Endpoint,
        wait_queue: Arc<WaitQueue>,
    ) -> Result<BoundNetlink<P::Message>, SystemError> {
        let message_queue = MessageQueue::<P::Message>::new();

        let bound_handle = {
            let endpoint = {
                let mut endpoint = NetlinkSocketAddr::new_unspecified();
                endpoint.add_groups(self.groups);
                endpoint
            };
            let receiver = MessageReceiver::new(message_queue.clone(), wait_queue);
            <P as SupportedNetlinkProtocol>::bind(&endpoint, receiver)?
        };

        Ok(BoundNetlink::new(bound_handle, message_queue))
    }

    fn check_io_events(&self) -> EPollEventType {
        EPollEventType::EPOLLOUT
    }
}
