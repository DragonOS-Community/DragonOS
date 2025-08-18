//! # Netlink route kernel module
//! 内核对于 Netlink 路由的处理模块

use crate::net::socket::netlink::{
    message::{
        segment::{ack::ErrorSegment, CSegmentType},
        ProtocolSegment,
    },
    route::message::{segment::RouteNlSegment, RouteNlMessage},
    table::{NetlinkRouteProtocol, SupportedNetlinkProtocol},
};
use core::marker::PhantomData;

mod addr;
mod utils;

pub(super) struct NetlinkRouteKernelSocket {
    _private: PhantomData<()>,
}

impl NetlinkRouteKernelSocket {
    const fn new() -> Self {
        NetlinkRouteKernelSocket {
            _private: PhantomData,
        }
    }

    pub(super) fn request(&self, request: &RouteNlMessage, dst_port: u32) {
        for segment in request.segments() {
            let header = segment.header();

            let seg_type = CSegmentType::try_from(header.type_).unwrap();
            let responce = match segment {
                RouteNlSegment::GetAddr(request) => addr::do_get_addr(request),
                RouteNlSegment::GetRoute(_new_route) => todo!(),
                _ => {
                    log::warn!("Unsupported route request segment type: {:?}", seg_type);
                    todo!()
                }
            };

            let responce = match responce {
                Ok(segments) => RouteNlMessage::new(segments),
                Err(error) => {
                    //todo 处理 `NetlinkMessageCommonFlags::ACK`
                    let err_segment = ErrorSegment::new_from_request(header, Some(error));
                    RouteNlMessage::new(vec![RouteNlSegment::Error(err_segment)])
                }
            };

            NetlinkRouteProtocol::unicast(dst_port, responce).unwrap();
        }
    }
}

/// 负责处理 Netlink 路由相关的内核模块
/// todo net namespace 实现之后应该是每一个 namespace 都有一个独立的 NetlinkRouteKernelSocket
static NETLINK_ROUTE_KERNEL: NetlinkRouteKernelSocket = NetlinkRouteKernelSocket::new();

pub(super) fn netlink_route_kernel() -> &'static NetlinkRouteKernelSocket {
    &NETLINK_ROUTE_KERNEL
}
