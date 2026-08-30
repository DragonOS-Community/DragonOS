//! # Netlink route kernel module
//! 内核对于 Netlink 路由的处理模块

use crate::{
    net::socket::netlink::{
        message::{
            segment::{ack::ErrorSegment, header::SegHdrCommonFlags, CSegmentType},
            ProtocolSegment,
        },
        route::message::{segment::RouteNlSegment, RouteNlMessage},
        table::{
            NetlinkKernelSocket, NetlinkRouteProtocol, StandardNetlinkProtocol,
            SupportedNetlinkProtocol,
        },
    },
    process::{
        cred::{CAPFlags, Cred},
        namespace::net_namespace::NetNamespace,
    },
};
use alloc::sync::Arc;
use core::marker::PhantomData;
use system_error::SystemError;

mod addr;
mod link;
mod neigh;
mod route;
mod utils;

pub(crate) use link::notify_link_change;

/// Per-send authorization and routing state for userspace rtnetlink requests.
///
/// Linux checks both the sending task's credentials and, unless an explicit
/// destination was supplied, the credentials captured when the socket was
/// opened. Both checks target the user namespace that owns the socket netns.
pub(super) struct RtnlRequestContext {
    sender_cred: Arc<Cred>,
    opener_cred: Arc<Cred>,
    netns: Arc<NetNamespace>,
    port_id: u32,
    destination_is_explicit: bool,
}

impl RtnlRequestContext {
    pub(super) fn new(
        sender_cred: Arc<Cred>,
        opener_cred: Arc<Cred>,
        netns: Arc<NetNamespace>,
        port_id: u32,
        destination_is_explicit: bool,
    ) -> Self {
        Self {
            sender_cred,
            opener_cred,
            netns,
            port_id,
            destination_is_explicit,
        }
    }

    pub(super) fn require_net_admin(&self) -> Result<(), SystemError> {
        let target_user_ns = self.netns.user_ns();
        if (!self.destination_is_explicit
            && !self
                .opener_cred
                .has_capability_in_ns(target_user_ns, CAPFlags::CAP_NET_ADMIN))
            || !self
                .sender_cred
                .has_capability_in_ns(target_user_ns, CAPFlags::CAP_NET_ADMIN)
        {
            return Err(SystemError::EPERM);
        }
        Ok(())
    }

    pub(super) fn netns(&self) -> Arc<NetNamespace> {
        self.netns.clone()
    }

    pub(super) fn port_id(&self) -> u32 {
        self.port_id
    }
}

/// 负责处理 Netlink 路由相关的内核模块
/// 每个 net namespace 都有一个独立的 NetlinkRouteKernelSocket
#[derive(Debug)]
pub struct NetlinkRouteKernelSocket {
    _private: PhantomData<()>,
}

impl NetlinkRouteKernelSocket {
    pub const fn new() -> Self {
        NetlinkRouteKernelSocket {
            _private: PhantomData,
        }
    }

    pub(super) fn request(&self, request: &RouteNlMessage, context: &RtnlRequestContext) {
        let dst_port = context.port_id();
        let netns = context.netns();
        for segment in request.segments() {
            let header = segment.header();

            let Ok(seg_type) = CSegmentType::try_from(header.type_) else {
                let err_segment = ErrorSegment::new_from_request(header, Some(SystemError::EINVAL));
                let err_msg = RouteNlMessage::new(vec![RouteNlSegment::Error(err_segment)]);
                if let Err(e) = NetlinkRouteProtocol::unicast(dst_port, err_msg, netns.clone()) {
                    log::warn!(
                        "netlink route: failed to deliver EINVAL ack to port {}: {:?}",
                        dst_port,
                        e
                    );
                }
                continue;
            };

            let request_flags = SegHdrCommonFlags::from_bits_truncate(header.flags);
            let need_ack = request_flags.contains(SegHdrCommonFlags::ACK);

            let response_segments = match segment {
                RouteNlSegment::GetAddr(request) => addr::do_get_addr(request, netns.clone()),
                RouteNlSegment::NewAddr(request) => addr::do_new_addr(request, netns.clone()),
                RouteNlSegment::DelAddr(request) => addr::do_del_addr(request, netns.clone()),
                RouteNlSegment::GetLink(request) => link::do_get_link(request, netns.clone()),
                RouteNlSegment::SetLink(request) if seg_type == CSegmentType::DELLINK => {
                    link::do_del_link(request, netns.clone())
                }
                RouteNlSegment::SetLink(request) => link::do_set_link(request, netns.clone()),
                RouteNlSegment::GetRoute(request) if seg_type == CSegmentType::GETRULE => {
                    route::do_get_rule(request, netns.clone())
                }
                RouteNlSegment::GetRoute(request) => route::do_get_route(request, netns.clone()),
                RouteNlSegment::NewRoute(request) => route::do_new_route(request, netns.clone()),
                RouteNlSegment::DelRoute(request) => route::do_del_route(request, netns.clone()),
                RouteNlSegment::NewNeigh(request) => neigh::do_new_neigh(request, netns.clone()),
                RouteNlSegment::GetNeigh(request) => neigh::do_get_neigh(request, netns.clone()),
                RouteNlSegment::DelNeigh(request) => neigh::do_del_neigh(request, netns.clone()),
                _ => {
                    log::warn!("Unsupported route request segment type: {:?}", seg_type);
                    Err(SystemError::EOPNOTSUPP_OR_ENOTSUP)
                }
            };

            let response = match response_segments {
                Ok(mut segments) => {
                    if segments.is_empty() {
                        if !need_ack {
                            continue;
                        }
                        let ack_segment = ErrorSegment::new_from_request(header, None);
                        segments.push(RouteNlSegment::Error(ack_segment));
                    }
                    RouteNlMessage::new(segments)
                }
                Err(error) => {
                    let err_segment = ErrorSegment::new_from_request(header, Some(error));
                    RouteNlMessage::new(vec![RouteNlSegment::Error(err_segment)])
                }
            };

            if let Err(e) = NetlinkRouteProtocol::unicast(dst_port, response, netns.clone()) {
                log::warn!(
                    "netlink route: failed to deliver response to port {}: {:?}",
                    dst_port,
                    e
                );
            }
        }
    }
}

impl NetlinkKernelSocket for NetlinkRouteKernelSocket {
    fn protocol(&self) -> StandardNetlinkProtocol {
        StandardNetlinkProtocol::ROUTE
    }

    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }
}
