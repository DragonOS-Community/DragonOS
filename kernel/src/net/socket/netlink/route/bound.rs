use crate::{
    filesystem::epoll::EPollEventType,
    libs::align::align_up,
    net::socket::{
        netlink::{
            addr::NetlinkSocketAddr,
            common::bound::BoundNetlink,
            message::{
                segment::{
                    ack::ErrorSegment,
                    header::{CMsgSegHdr, SegHdrCommonFlags},
                    CSegmentType,
                },
                ProtocolSegment, NLMSG_ALIGN,
            },
            route::{
                kern::{NetlinkRouteKernelSocket, RtnlRequestContext},
                message::RouteNlMessage,
            },
            table::{NetlinkRouteProtocol, SupportedNetlinkProtocol},
        },
        utils::datagram_common,
        PMSG,
    },
    process::{namespace::net_namespace::NetNamespace, ProcessManager},
};
use alloc::sync::Arc;
use core::mem::size_of;
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
        destination_is_explicit: bool,
    ) -> Result<usize, SystemError> {
        if *to != NetlinkSocketAddr::new_unspecified() {
            return Err(SystemError::ENOTCONN);
        }

        let sum_lens = buf.len();
        let local_port = self.handle.port();
        let netns = self.netns();
        let context = RtnlRequestContext::new(
            ProcessManager::current_pcb().cred(),
            self.opener_cred(),
            netns.clone(),
            local_port,
            destination_is_explicit,
        );

        let Some(route_kernel) = netns.get_netlink_kernel_socket_by_protocol(
            crate::net::socket::netlink::table::StandardNetlinkProtocol::ROUTE.into(),
        ) else {
            log::warn!("No route kernel socket available in net namespace");
            return Err(SystemError::ECONNREFUSED);
        };

        let route_kernel_socket = route_kernel
            .as_any_ref()
            .downcast_ref::<NetlinkRouteKernelSocket>()
            .ok_or(SystemError::EINVAL)?;

        let mut offset = 0usize;
        while offset < buf.len() {
            let slice = &buf[offset..];
            if slice.len() < size_of::<CMsgSegHdr>() {
                return Err(SystemError::EINVAL);
            }

            // SAFETY: `slice` has at least `size_of::<CMsgSegHdr>()` bytes.
            let header = unsafe { core::ptr::read_unaligned(slice.as_ptr() as *const CMsgSegHdr) };
            let msg_len = header.len as usize;
            if msg_len < size_of::<CMsgSegHdr>() || offset + msg_len > buf.len() {
                return Err(SystemError::EINVAL);
            }

            let msg_buf = &buf[offset..offset + msg_len];
            offset += align_up(msg_len, NLMSG_ALIGN);

            let flags = SegHdrCommonFlags::from_bits_truncate(header.flags);
            if !flags.contains(SegHdrCommonFlags::REQUEST) {
                continue;
            }

            let payload_len = msg_len - size_of::<CMsgSegHdr>();
            if payload_len == 0 && is_rtnl_message_type(header.type_) {
                if flags.contains(SegHdrCommonFlags::ACK) {
                    send_route_ack(&header, None, local_port, netns.clone());
                }
                continue;
            }

            if rtnl_requires_net_admin(header.type_) {
                if let Err(error) = context.require_net_admin() {
                    send_route_ack(&header, Some(error), local_port, netns.clone());
                    continue;
                }
            }

            if CSegmentType::try_from(header.type_).is_err() {
                send_route_ack(
                    &header,
                    Some(SystemError::EOPNOTSUPP_OR_ENOTSUP),
                    local_port,
                    netns.clone(),
                );
                continue;
            }

            let segment = match RouteNlMessage::read_from(msg_buf) {
                Ok(msg) => {
                    if msg.segments().len() != 1 {
                        return Err(SystemError::EINVAL);
                    }
                    msg.segments()[0].clone()
                }
                Err(e) => {
                    log::warn!(
                        "netlink_send: failed to read netlink message from buffer: {:?}",
                        e
                    );
                    return Err(e);
                }
            };

            let mut nlmsg = RouteNlMessage::new(vec![segment]);
            for seg in nlmsg.segments_mut() {
                let hdr = seg.header_mut();
                if hdr.pid == 0 {
                    hdr.pid = local_port;
                }
            }

            route_kernel_socket.request(&nlmsg, &context);

            // gVisor 测例常以 sizeof(req) 发送，nlmsg_len 之后多为结构体尾部零填充，勿当作下一条消息。
            if offset < buf.len() && buf[offset..].iter().all(|&b| b == 0) {
                break;
            }
        }

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

        let remote = NetlinkSocketAddr::new_unspecified();

        Ok((copied, orig_len, remote))
    }

    fn check_io_events(&self) -> EPollEventType {
        self.check_io_events_common()
    }
}

/// Mirrors Linux `rtnl_msgtype_kind()`: RTM message kinds repeat in groups of
/// four as NEW, DEL, GET, SET. Every non-GET userspace request in Linux's RTM
/// range requires CAP_NET_ADMIN, including message types DragonOS does not yet
/// implement. Linux's zero-payload success path is handled before this helper.
fn rtnl_requires_net_admin(message_type: u16) -> bool {
    const RTM_GET_KIND: u16 = 2;

    is_rtnl_message_type(message_type) && ((message_type - RTM_BASE) & 3) != RTM_GET_KIND
}

const RTM_BASE: u16 = 16;
const RTM_MAX: u16 = 123;

fn is_rtnl_message_type(message_type: u16) -> bool {
    (RTM_BASE..=RTM_MAX).contains(&message_type)
}

fn send_route_ack(
    request_header: &CMsgSegHdr,
    error: Option<SystemError>,
    dst_port: u32,
    netns: Arc<NetNamespace>,
) {
    use crate::net::socket::netlink::route::message::segment::RouteNlSegment;

    let err_segment = ErrorSegment::new_from_request(request_header, error);
    let err_msg = RouteNlMessage::new(vec![RouteNlSegment::Error(err_segment)]);
    if let Err(e) = NetlinkRouteProtocol::unicast(dst_port, err_msg, netns) {
        log::warn!(
            "netlink route: failed to deliver ack to port {}: {:?}",
            dst_port,
            e
        );
    }
}
