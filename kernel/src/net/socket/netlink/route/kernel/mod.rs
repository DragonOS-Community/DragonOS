//! # Netlink route kernel module
//! 内核对于 Netlink 路由的处理模块

use crate::net::socket::netlink::route::message::RouteNlMessage;
use core::marker::PhantomData;

pub(super) struct NetlinkRouteKernelSocket {
    _private: PhantomData<()>,
}

impl NetlinkRouteKernelSocket {
    const fn new() -> Self {
        NetlinkRouteKernelSocket {
            _private: PhantomData,
        }
    }

    pub(super) fn request(&self, request: &RouteNlMessage, dst_port: u32) {}
}

/// 负责处理 Netlink 路由相关的内核模块
/// todo net namespace 实现之后应该是每一个 namespace 都有一个独立的 NetlinkRouteKernelSocket
static NETLINK_ROUTE_KERNEL: NetlinkRouteKernelSocket = NetlinkRouteKernelSocket::new();

pub(super) fn netlink_route_kernel() -> &'static NetlinkRouteKernelSocket {
    &NETLINK_ROUTE_KERNEL
}
