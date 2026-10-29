use super::NetfilterKernelSocket;
use crate::{
    filesystem::epoll::EPollEventType,
    net::socket::{
        netlink::{
            addr::NetlinkSocketAddr,
            common::bound::BoundNetlink,
            table::{NetlinkNetfilterProtocol, StandardNetlinkProtocol, SupportedNetlinkProtocol},
        },
        utils::datagram_common::Bound,
        PMSG,
    },
};
use system_error::SystemError;

impl Bound for BoundNetlink<NetlinkNetfilterProtocol> {
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
        bytes: &[u8],
        to: &Self::Endpoint,
        flags: PMSG,
        explicit: bool,
    ) -> Result<usize, SystemError> {
        if flags.contains(PMSG::OOB) {
            return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
        }
        if bytes.is_empty() {
            return Err(SystemError::ENODATA);
        }
        NetlinkNetfilterProtocol::check_connect(to, &self.netns)?;
        let endpoint = self
            .netns
            .get_netlink_kernel_socket_by_protocol(StandardNetlinkProtocol::NETFILTER.into())
            .ok_or(SystemError::ECONNREFUSED)?;
        let kernel = endpoint
            .as_any_ref()
            .downcast_ref::<NetfilterKernelSocket>()
            .ok_or(SystemError::EINVAL)?;
        kernel.request(
            bytes,
            self.handle.port(),
            self.netns(),
            &self.opener_cred(),
            explicit,
            &self.protocol_state,
        )?;
        Ok(bytes.len())
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
