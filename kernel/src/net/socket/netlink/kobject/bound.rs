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
use alloc::vec::Vec;
use system_error::SystemError;

fn send_kobject_message(
    message: KobjectUeventMessage,
    sent_len: usize,
    to: &NetlinkSocketAddr,
    netns: alloc::sync::Arc<crate::process::namespace::net_namespace::NetNamespace>,
) -> Result<usize, SystemError> {
    if to.port() != 0 {
        <NetlinkKobjectUeventProtocol as SupportedNetlinkProtocol>::unicast(
            to.port(),
            message.clone(),
            netns.clone(),
        )?;
    }

    if !to.groups().is_empty() {
        <NetlinkKobjectUeventProtocol as SupportedNetlinkProtocol>::multicast(
            to.groups(),
            message,
            netns,
        )?;
    }

    Ok(sent_len)
}

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
        let message = KobjectUeventMessage::try_new(buf)?;
        send_kobject_message(message, sent_len, to, self.netns())
    }

    fn try_send_vec(
        &self,
        buf: Vec<u8>,
        to: &Self::Endpoint,
        _flags: PMSG,
    ) -> Result<usize, SystemError> {
        let sent_len = buf.len();
        let message = KobjectUeventMessage::from_vec(buf);
        send_kobject_message(message, sent_len, to, self.netns())
    }

    fn try_recv(
        &self,
        writer: &mut [u8],
        flags: PMSG,
    ) -> Result<(usize, usize, Self::Endpoint), SystemError> {
        let mut receive_queue = self.receive_queue.0.lock();
        let Some(message) = receive_queue.front() else {
            return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
        };

        let orig_len = message.as_bytes().len();
        let copied = writer.len().min(orig_len);
        if copied > 0 {
            writer[..copied].copy_from_slice(&message.as_bytes()[..copied]);
        }

        if !flags.contains(PMSG::PEEK) {
            receive_queue.pop_front();
        }

        Ok((copied, orig_len, NetlinkSocketAddr::new_unspecified()))
    }

    fn check_io_events(&self) -> EPollEventType {
        self.check_io_events_common()
    }
}
