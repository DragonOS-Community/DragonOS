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

pub(in crate::net::socket::netlink) fn send_userspace(
    bytes: Vec<u8>,
    to: NetlinkSocketAddr,
    netns: alloc::sync::Arc<crate::process::namespace::net_namespace::NetNamespace>,
    nonblocking: bool,
    timeout: Option<crate::time::Duration>,
) -> Result<usize, SystemError> {
    let sent_len = bytes.len();
    let message = KobjectUeventMessage::try_from_vec(bytes)?;
    if to.port() != 0 {
        let started = crate::time::Instant::now();
        loop {
            // Pin while attaching/waiting, not while holding the port table.
            // Linux netlink_unicast repeats lookup after attachskb wakes. A
            // replacement port may therefore accept the retried datagram.
            let receiver = NetlinkKobjectUeventProtocol::receiver(to.port(), netns.clone())?;
            let remaining = timeout.map(|budget| {
                crate::time::Duration::from_micros(
                    budget.total_micros().saturating_sub(
                        crate::time::Instant::now()
                            .saturating_sub(started)
                            .total_micros(),
                    ),
                )
            });
            match receiver.enqueue_user(message.clone(), nonblocking, remaining) {
                Err(SystemError::ECONNREFUSED) => continue,
                result => {
                    result?;
                    break;
                }
            }
        }
    }
    if !to.groups().is_empty() {
        NetlinkKobjectUeventProtocol::multicast(to.groups(), message, netns)?;
    }
    Ok(sent_len)
}

impl datagram_common::Bound for BoundNetlink<NetlinkKobjectUeventProtocol> {
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
        _destination_is_explicit: bool,
    ) -> Result<usize, SystemError> {
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(buf.len())
            .map_err(|_| SystemError::ENOMEM)?;
        bytes.extend_from_slice(buf);
        send_userspace(bytes, *to, self.netns(), true, None)
    }

    fn try_send_vec(
        &self,
        buf: Vec<u8>,
        to: &Self::Endpoint,
        _flags: PMSG,
        _destination_is_explicit: bool,
    ) -> Result<usize, SystemError> {
        send_userspace(buf, *to, self.netns(), true, None)
    }

    fn try_recv(
        &self,
        writer: &mut [u8],
        flags: PMSG,
    ) -> Result<(usize, usize, Self::Endpoint), SystemError> {
        self.try_recv_common(writer, flags)
    }

    fn check_io_events(&self) -> EPollEventType {
        self.check_io_events_common()
    }
}
