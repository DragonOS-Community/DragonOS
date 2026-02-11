use crate::{
    filesystem::epoll::EPollEventType,
    net::socket::{
        netlink::{
            addr::NetlinkSocketAddr,
            common::bound::BoundNetlink,
            message::ProtocolSegment,
            route::{kern::NetlinkRouteKernelSocket, message::RouteNlMessage},
        },
        utils::datagram_common,
        PMSG,
    },
};
use system_error::SystemError;

impl datagram_common::Bound for BoundNetlink<RouteNlMessage> {
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
        _flags: crate::net::socket::PMSG,
    ) -> Result<usize, SystemError> {
        if *to != NetlinkSocketAddr::new_unspecified() {
            return Err(SystemError::ENOTCONN);
        }

        let sum_lens = buf.len();

        let mut nlmsg = match RouteNlMessage::read_from(buf) {
            Ok(msg) => msg,
            Err(e) if e == SystemError::EFAULT => {
                // 说明这个时候 buf 不是一个完整的 netlink 消息
                return Err(e);
            }
            Err(e) => {
                log::warn!(
                    "netlink_send: failed to read netlink message from buffer: {:?}",
                    e
                );
                return Err(e);
            }
        };

        let local_port = self.handle.port();

        for segment in nlmsg.segments_mut() {
            let header = segment.header_mut();
            if header.pid == 0 {
                header.pid = local_port;
            }
        }

        let Some(route_kernel) = self
            .netns
            .get_netlink_kernel_socket_by_protocol(nlmsg.protocol().into())
        else {
            log::warn!("No route kernel socket available in net namespace");
            return Ok(sum_lens);
        };

        let route_kernel_socket = route_kernel
            .as_any_ref()
            .downcast_ref::<NetlinkRouteKernelSocket>()
            .unwrap();

        route_kernel_socket.request(&nlmsg, local_port, self.netns());

        Ok(sum_lens)
    }

    fn try_recv(
        &self,
        writer: &mut [u8],
        flags: crate::net::socket::PMSG,
    ) -> Result<(usize, usize, Self::Endpoint), SystemError> {
        let mut receive_queue = self.receive_queue.0.lock();

        let Some(res) = receive_queue.front() else {
            return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
        };

        let orig_len = res.total_len();
        let copied = if writer.len() >= orig_len {
            res.write_to(writer)?
        } else {
            let mut full = alloc::vec![0u8; orig_len];
            let written = res.write_to(&mut full)?;
            let copy_len = written.min(writer.len());
            if copy_len > 0 {
                writer[..copy_len].copy_from_slice(&full[..copy_len]);
            }
            copy_len
        };

        if !flags.contains(PMSG::PEEK) {
            receive_queue.pop_front();
        }

        // todo 目前这个信息只能来自内核
        let remote = NetlinkSocketAddr::new_unspecified();

        Ok((copied, orig_len, remote))
    }

    fn check_io_events(&self) -> EPollEventType {
        self.check_io_events_common()
    }
}
