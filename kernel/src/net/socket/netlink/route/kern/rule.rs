use alloc::{sync::Arc, vec::Vec};
use smoltcp::wire::Ipv6Cidr;
use system_error::SystemError;

use crate::{
    net::{
        route::{self, Ipv6Rule, Ipv6RuleSelector},
        rtnl::RtnlGuard,
        socket::{
            netlink::{
                message::segment::{
                    header::{CMsgSegHdr, NewRequestFlags, SegHdrCommonFlags},
                    CSegmentType,
                },
                route::message::{
                    segment::{
                        route::RouteTable,
                        rule::{RuleAttr, RuleSegment, RuleSegmentBody},
                        RouteNlSegment,
                    },
                    RouteNlMessage,
                },
                table::{NetlinkRouteProtocol, SupportedNetlinkProtocol},
            },
            AddressFamily,
        },
    },
    process::namespace::net_namespace::NetNamespace,
};

use super::{deliver_response, utils::multicast_notify_excluding, RtnlRequestContext};

const FR_ACT_TO_TBL: u8 = 1;
const RTNLGRP_IPV6_RULE: u32 = 19;
const RTMGRP_IPV6_RULE: u32 = 1 << (RTNLGRP_IPV6_RULE - 1);

pub(super) fn change(
    rtnl: &RtnlGuard,
    request: &RuleSegment,
    context: &RtnlRequestContext,
    kind: CSegmentType,
) -> Result<Vec<RouteNlSegment>, SystemError> {
    let netns = context.netns();
    let body = request.body();
    if body.family != AddressFamily::INet6 as u8 {
        return Err(SystemError::EAFNOSUPPORT);
    }
    if body.src_len > 128 {
        return Err(SystemError::EINVAL);
    }
    let adding = kind == CSegmentType::NEWRULE;
    if body.dst_len != 0
        || body.tos != 0
        || body.flags != 0
        || !(body.action == FR_ACT_TO_TBL || !adding && body.action == 0)
    {
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }

    let mut table = u32::from(body.table);
    let mut priority = None;
    let mut address = None;
    let mut protocol = None;
    for attr in request.attrs() {
        match *attr {
            RuleAttr::Source(bytes) => address = Some(bytes.into()),
            RuleAttr::Priority(bytes) => priority = Some(u32::from_ne_bytes(bytes)),
            RuleAttr::Table(bytes) => table = u32::from_ne_bytes(bytes),
            RuleAttr::Protocol(value) => protocol = Some(value),
            RuleAttr::SuppressPrefixLength(bytes) => {
                if u32::from_ne_bytes(bytes) != u32::MAX {
                    return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
                }
            }
        }
    }
    let source = if body.src_len == 0 {
        None
    } else {
        Some(Ipv6Cidr::new(
            address.ok_or(SystemError::EINVAL)?,
            body.src_len,
        ))
    };
    let selector = Ipv6RuleSelector {
        priority,
        table: (table != 0).then_some(table),
        source,
        protocol: protocol.filter(|value| *value != 0),
    };

    let exclusive =
        NewRequestFlags::from_bits_truncate(request.header().flags).contains(NewRequestFlags::EXCL);
    let applied = if adding {
        route::insert_ipv6_rule(rtnl, &netns, selector, exclusive)?
    } else {
        route::remove_ipv6_rule(rtnl, &netns, selector)?
    };
    notify_change(request.header(), context.port_id(), netns, kind, applied);
    Ok(Vec::new())
}

fn notify_change(
    request: &CMsgSegHdr,
    port: u32,
    netns: Arc<NetNamespace>,
    kind: CSegmentType,
    applied: Ipv6Rule,
) {
    let header = CMsgSegHdr {
        pid: port,
        ..*request
    };
    let echo =
        SegHdrCommonFlags::from_bits_truncate(request.flags).contains(SegHdrCommonFlags::ECHO);
    let excluded = if echo { port } else { 0 };
    match rule_segment(header, kind, applied) {
        Ok(segment) => {
            multicast_notify_excluding(netns.clone(), RTMGRP_IPV6_RULE, excluded, segment)
        }
        Err(_) => {
            NetlinkRouteProtocol::report_group_overrun(RTNLGRP_IPV6_RULE, excluded, netns.clone())
        }
    }
    if echo {
        let mut reply = Vec::new();
        match (
            reply.try_reserve_exact(1),
            rule_segment(header, kind, applied),
        ) {
            (Ok(()), Ok(segment)) => {
                reply.push(segment);
                deliver_response(port, RouteNlMessage::new(reply), netns);
            }
            _ => NetlinkRouteProtocol::report_overrun(port, netns),
        }
    }
}

pub(super) fn rule_segment(
    mut header: CMsgSegHdr,
    kind: CSegmentType,
    rule: Ipv6Rule,
) -> Result<RouteNlSegment, SystemError> {
    header.type_ = kind as u16;
    header.flags = 0;
    let body = RuleSegmentBody {
        family: AddressFamily::INet6 as u8,
        table: u8::try_from(rule.table).unwrap_or(RouteTable::Compat as u8),
        src_len: rule.source.map_or(0, |source| source.prefix_len()),
        action: FR_ACT_TO_TBL,
        ..RuleSegmentBody::default()
    };
    let mut attrs = Vec::new();
    attrs
        .try_reserve_exact(5)
        .map_err(|_| SystemError::ENOMEM)?;
    attrs.push(RuleAttr::Table(rule.table.to_ne_bytes()));
    attrs.push(RuleAttr::SuppressPrefixLength(u32::MAX.to_ne_bytes()));
    attrs.push(RuleAttr::Protocol(rule.protocol));
    if rule.priority != 0 {
        attrs.push(RuleAttr::Priority(rule.priority.to_ne_bytes()));
    }
    if let Some(source) = rule.source {
        attrs.push(RuleAttr::Source(source.address().octets()));
    }
    let segment = RuleSegment::new(header, body, attrs);
    Ok(if kind == CSegmentType::DELRULE {
        RouteNlSegment::DelRule(segment)
    } else {
        RouteNlSegment::NewRule(segment)
    })
}
