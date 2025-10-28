//! # Netlink route kernel module
//! 内核对于 Netlink 路由的处理模块

use crate::{
    net::socket::netlink::{
        message::{
            segment::{ack::ErrorSegment, CSegmentType},
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

            let seg_type = CSegmentType::try_from(header.type_).unwrap();
            let responce = match segment {
                RouteNlSegment::GetAddr(request) => addr::do_get_addr(request, netns.clone()),
                RouteNlSegment::GetLink(request) => link::do_get_link(request, netns.clone()),
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

            NetlinkRouteProtocol::unicast(dst_port, responce, netns.clone()).unwrap();
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
