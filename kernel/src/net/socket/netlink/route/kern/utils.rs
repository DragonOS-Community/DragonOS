use crate::net::socket::netlink::{
    addr::multicast::GroupIdSet,
    message::{
        segment::{
            ack::DoneSegment,
            header::{CMsgSegHdr, SegHdrCommonFlags},
            CSegmentType,
        },
        Message, ProtocolSegment,
    },
    route::message::segment::RouteNlSegment,
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

pub fn finish_response(
    request_header: &CMsgSegHdr,
    dump_all: bool,
    response_segments: &mut Vec<RouteNlSegment>,
) {
    if !dump_all {
        if response_segments.len() != 1 {
            log::warn!(
                "netlink route: expected exactly one response segment, got {}",
                response_segments.len()
            );
        }
        return;
    }

    append_done_segment(request_header, response_segments);
    add_multi_flag(response_segments);
}

fn append_done_segment(header: &CMsgSegHdr, response_segments: &mut Vec<RouteNlSegment>) {
    let done_segment = DoneSegment::new_from_request(header, None);
    response_segments.push(RouteNlSegment::Done(done_segment));
}

fn add_multi_flag(responce_segment: &mut [RouteNlSegment]) {
    for segment in responce_segment.iter_mut() {
        let header = segment.header_mut();
        let mut flags = SegHdrCommonFlags::from_bits_truncate(header.flags);
        flags |= SegHdrCommonFlags::MULTI;
        header.flags = flags.bits();
    }
}

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
    if let Err(e) = NetlinkRouteProtocol::multicast(
        GroupIdSet::new(group_mask),
        Message::new(vec![segment]),
        netns,
    ) {
        log::warn!("netlink route: multicast notify failed: {:?}", e);
    }
}
