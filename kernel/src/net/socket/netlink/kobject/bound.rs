use crate::{
    filesystem::epoll::EPollEventType,
    net::socket::{
        netlink::{
            addr::NetlinkSocketAddr,
            common::bound::BoundNetlink,
            kobject::message::KobjectUeventMessage,
            table::{NetlinkKobjectUeventProtocol, SupportedNetlinkProtocol},
        },
        utils::datagram_common,
        PMSG,
    },
};
use system_error::SystemError;

impl datagram_common::Bound for BoundNetlink<KobjectUeventMessage> {
    type Endpoint = NetlinkSocketAddr;

    fn bind(&mut self, endpoint: &Self::Endpoint) -> Result<(), SystemError> {
        self.bind_common(endpoint)
    }

    fn local_endpoint(&self) -> Self::Endpoint {
        self.handle.addr()
    }

    fn remote_endpoint(&self) -> Option<Self::Endpoint> {
        Some(self.remote_addr)
    }

    fn set_remote_endpoint(&mut self, endpoint: &Self::Endpoint) {
        self.remote_addr = *endpoint;
    }

    fn try_send(
        &self,
        buf: &[u8],
        to: &Self::Endpoint,
        _flags: PMSG,
    ) -> Result<usize, SystemError> {
        let sent_len = buf.len();
        let message = KobjectUeventMessage::new(buf);

        if to.port() != 0 {
            <NetlinkKobjectUeventProtocol as SupportedNetlinkProtocol>::unicast(
                to.port(),
                message.clone(),
                self.netns(),
            )?;
        }

        if !to.groups().is_empty() {
            <NetlinkKobjectUeventProtocol as SupportedNetlinkProtocol>::multicast(
                to.groups(),
                message,
                self.netns(),
            )?;
        }

        Ok(sent_len)
    }

    fn try_recv(
        &self,
        writer: &mut [u8],
        flags: PMSG,
    ) -> Result<(usize, Self::Endpoint), SystemError> {
        let mut receive_queue = self.receive_queue.0.lock();
        let Some(message) = receive_queue.front() else {
            return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
        };

        let copied = writer.len().min(message.as_bytes().len());
        if copied > 0 {
            writer[..copied].copy_from_slice(&message.as_bytes()[..copied]);
        }

        if !flags.contains(PMSG::PEEK) {
            receive_queue.pop_front();
        }

        Ok((copied, NetlinkSocketAddr::new_unspecified()))
    }

    fn check_io_events(&self) -> EPollEventType {
        self.check_io_events_common()
    }
}
