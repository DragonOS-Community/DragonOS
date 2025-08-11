use crate::net::socket::netlink::{
    addr::{multicast::GroupIdSet, NetlinkSocketAddr},
    receiver::MessageQueue,
    table::BoundHandle,
};
use alloc::fmt::Debug;
use system_error::SystemError;

#[derive(Debug)]
pub struct BoundNetlink<Message: 'static + Debug> {
    pub(in crate::net::socket::netlink) handle: BoundHandle<Message>,
    pub(in crate::net::socket::netlink) remote_addr: NetlinkSocketAddr,
    pub(in crate::net::socket::netlink) receive_queue: MessageQueue<Message>,
}

impl<Message: 'static + Debug> BoundNetlink<Message> {
    pub(super) fn new(handle: BoundHandle<Message>, message_queue: MessageQueue<Message>) -> Self {
        Self {
            handle,
            remote_addr: NetlinkSocketAddr::new_unspecified(),
            receive_queue: message_queue,
        }
    }

    pub fn bind_common(&mut self, endpoint: &NetlinkSocketAddr) -> Result<(), SystemError> {
        if endpoint.port() != self.handle.port() {
            return Err(SystemError::EINVAL);
        }
        let groups = endpoint.groups();
        self.handle.bind_groups(groups);

        Ok(())
    }

    // pub fn check_io_events_common(&self) -> EPollEventType {
    //     let mut events = EPollEventType::EPOLLOUT;

    //     let receive_queue = self.receive_queue.0.lock();
    //     if !receive_queue.is_empty() {
    //         events |= EPollEventType::EPOLLIN;
    //     }

    //     events
    // }

    pub(super) fn add_groups(&mut self, groups: GroupIdSet) {
        self.handle.add_groups(groups);
    }

    pub(super) fn drop_groups(&mut self, groups: GroupIdSet) {
        self.handle.drop_groups(groups);
    }
}
