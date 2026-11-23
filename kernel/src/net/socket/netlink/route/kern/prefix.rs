use super::utils::{kernel_notify_header, multicast_notify};
use crate::{
    net::socket::{
        netlink::{
            message::segment::CSegmentType,
            route::message::segment::{
                prefix::{PrefixAttr, PrefixSegment, PrefixSegmentBody},
                RouteNlSegment,
            },
            table::{NetlinkRouteProtocol, SupportedNetlinkProtocol},
        },
        AddressFamily,
    },
    process::namespace::net_namespace::NetNamespace,
};
use alloc::{sync::Arc, vec::Vec};
use smoltcp::wire::{NdiscOptionType, NdiscPrefixInformation};

const RTNLGRP_IPV6_PREFIX: u32 = 18;
const RTMGRP_IPV6_PREFIX: u32 = 1 << (RTNLGRP_IPV6_PREFIX - 1);

pub(crate) fn has_prefix_listeners(netns: &Arc<NetNamespace>) -> bool {
    NetlinkRouteProtocol::has_group_listeners(RTNLGRP_IPV6_PREFIX, netns.clone())
}

pub(crate) fn notify_ra_prefix(
    netns: &Arc<NetNamespace>,
    ifindex: u32,
    prefix: &NdiscPrefixInformation,
    flags: u8,
) {
    let body = PrefixSegmentBody {
        family: AddressFamily::INet6 as u8,
        pad1: 0,
        pad2: 0,
        ifindex: ifindex as i32,
        type_: u8::from(NdiscOptionType::PrefixInformation),
        prefix_len: prefix.prefix_len,
        flags,
        pad3: 0,
    };
    let mut attrs = Vec::new();
    if attrs.try_reserve_exact(2).is_err() {
        NetlinkRouteProtocol::report_group_overrun(RTNLGRP_IPV6_PREFIX, 0, netns.clone());
        return;
    }
    attrs.push(PrefixAttr::Address(prefix.prefix.octets()));
    attrs.push(PrefixAttr::cache_info(
        prefix.preferred_lifetime.secs() as u32,
        prefix.valid_lifetime.secs() as u32,
    ));
    let segment = PrefixSegment::new(kernel_notify_header(CSegmentType::NEWPREFIX), body, attrs);
    multicast_notify(
        netns.clone(),
        RTMGRP_IPV6_PREFIX,
        RouteNlSegment::NewPrefix(segment),
    );
}
