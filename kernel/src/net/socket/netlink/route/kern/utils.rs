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
use alloc::{sync::Arc, vec::Vec};

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
    multicast_notify_excluding(netns, group_mask, 0, segment);
}

pub(super) fn multicast_notify_excluding(
    netns: Arc<NetNamespace>,
    group_mask: u32,
    excluded_port: u32,
    segment: RouteNlSegment,
) {
    let groups = GroupIdSet::new(group_mask);
    let mut segments = Vec::new();
    if segments.try_reserve_exact(1).is_err() {
        for group_index in groups.ids_iter() {
            NetlinkRouteProtocol::report_group_overrun(
                group_index + 1,
                excluded_port,
                netns.clone(),
            );
        }
        return;
    }
    segments.push(segment);
    let message = Message::new(segments);
    for group_index in groups.ids_iter() {
        let group_id = group_index + 1;
        match RouteNlPacket::notification(&message, 1 << group_index) {
            Ok(packet) => {
                NetlinkRouteProtocol::notify_group(group_id, excluded_port, packet, netns.clone())
            }
            Err(_) => {
                NetlinkRouteProtocol::report_group_overrun(group_id, excluded_port, netns.clone())
            }
        }
    }
}
