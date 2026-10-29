use crate::net::socket::netlink::{
    addr::multicast::GroupIdSet,
    message::{
        segment::{
            header::{CMsgSegHdr, SegHdrCommonFlags},
            CSegmentType,
        },
        Message,
    },
    route::message::{segment::RouteNlSegment, RouteNlPacket},
    table::{NetlinkRouteProtocol, SupportedNetlinkProtocol},
};
use crate::process::namespace::net_namespace::NetNamespace;
use alloc::sync::Arc;

pub const RTMGRP_LINK: u32 = 0x1;
pub const RTMGRP_NEIGH: u32 = 0x4;
pub const RTMGRP_IPV4_IFADDR: u32 = 0x10;
pub const RTMGRP_IPV4_ROUTE: u32 = 0x40;
pub const RTMGRP_IPV6_IFADDR: u32 = 0x100;
pub const RTMGRP_IPV6_ROUTE: u32 = 0x400;

pub fn kernel_notify_header(type_: CSegmentType) -> CMsgSegHdr {
    CMsgSegHdr {
        len: 0,
        type_: type_ as u16,
        flags: SegHdrCommonFlags::empty().bits(),
        seq: 0,
        pid: 0,
    }
}

pub fn multicast_notify(netns: Arc<NetNamespace>, group_mask: u32, segment: RouteNlSegment) {
    let message = Message::new(vec![segment]);
    for group_index in GroupIdSet::new(group_mask).ids_iter() {
        let group_id = group_index + 1;
        match RouteNlPacket::notification(message.clone(), 1 << group_index) {
            Ok(packet) => NetlinkRouteProtocol::notify_group(group_id, 0, packet, netns.clone()),
            Err(_) => NetlinkRouteProtocol::report_group_overrun(group_id, 0, netns.clone()),
        }
    }
}
