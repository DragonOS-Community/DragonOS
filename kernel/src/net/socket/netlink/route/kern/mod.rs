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
    process::namespace::net_namespace::NetNamespace,
};
use alloc::sync::Arc;
use core::marker::PhantomData;
use system_error::SystemError;

mod addr;
mod link;
mod utils;

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

    pub(super) fn request(
        &self,
        request: &RouteNlMessage,
        dst_port: u32,
        netns: Arc<NetNamespace>,
    ) {
        for segment in request.segments() {
            let header = segment.header();

            let Ok(seg_type) = CSegmentType::try_from(header.type_) else {
                let err_segment = ErrorSegment::new_from_request(header, Some(SystemError::EINVAL));
                let err_msg = RouteNlMessage::new(vec![RouteNlSegment::Error(err_segment)]);
                let _ = NetlinkRouteProtocol::unicast(dst_port, err_msg, netns.clone());
                continue;
            };

            let request_flags = SegHdrCommonFlags::from_bits_truncate(header.flags);
            let need_ack = request_flags.contains(SegHdrCommonFlags::ACK);

            let response_segments = match segment {
                RouteNlSegment::GetAddr(request) => addr::do_get_addr(request, netns.clone()),
                RouteNlSegment::NewAddr(request) => addr::do_new_addr(request, netns.clone()),
                RouteNlSegment::DelAddr(request) => addr::do_del_addr(request, netns.clone()),
                RouteNlSegment::GetLink(request) => link::do_get_link(request, netns.clone()),
                RouteNlSegment::GetRoute(_new_route) => Err(SystemError::EOPNOTSUPP_OR_ENOTSUP),
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

            let _ = NetlinkRouteProtocol::unicast(dst_port, response, netns.clone());
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
