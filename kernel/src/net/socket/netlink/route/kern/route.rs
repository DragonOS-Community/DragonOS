use crate::{
    driver::net::types::InterfaceFlags,
    net::{
        route::{
            self, RouteDeleteSelector, RouteEntry, RouteMutationOutcome, RouteNewFlags,
            RTN_UNICAST, RTPROT_BOOT, RT_TABLE_MAIN,
        },
        rtnl::RtnlGuard,
        socket::{
            netlink::{
                message::segment::{
                    header::{CMsgSegHdr, NewRequestFlags},
                    CSegmentType,
                },
                route::{
                    kern::utils::{
                        kernel_notify_header, multicast_notify, RTMGRP_IPV4_ROUTE,
                        RTMGRP_IPV6_ROUTE,
                    },
                    message::{
                        attr::route::RouteAttr,
                        segment::{
                            route::{
                                RouteFlags, RouteProtocol, RouteScope, RouteSegment,
                                RouteSegmentBody, RouteTable, RouteType,
                            },
                            RouteNlSegment,
                        },
                    },
                },
            },
            AddressFamily,
        },
    },
    process::namespace::net_namespace::NetNamespace,
};
use alloc::{sync::Arc, vec, vec::Vec};
use smoltcp::wire::{IpAddress, IpCidr, Ipv4Address, Ipv4Cidr, Ipv6Address, Ipv6Cidr};
use system_error::SystemError;

const IP6_RT_PRIO_USER: u32 = 1024;
const RTN_MAX: u8 = 11;

fn effective_route_table(header: u8, attr: Option<u32>) -> u32 {
    let table = attr.unwrap_or(header as u32);
    if table == RouteTable::Unspec as u32 {
        RT_TABLE_MAIN
    } else {
        table
    }
}

pub(super) fn do_get_route(
    request: &RouteSegment,
    netns: Arc<NetNamespace>,
) -> Result<Vec<RouteNlSegment>, SystemError> {
    lookup_route(request, netns)
}

pub(super) fn do_new_route(
    rtnl: &RtnlGuard,
    request: &RouteSegment,
    netns: Arc<NetNamespace>,
) -> Result<Vec<RouteNlSegment>, SystemError> {
    validate_new_request(request)?;
    let parsed = ParsedRouteRequest::from_segment(request)?;
    let table = effective_route_table(request.body().table, parsed.table);
    let onlink = request.body().flags.contains(RouteFlags::ONLINK);
    let reject = request.body().family == AddressFamily::INet6
        && route::is_ipv6_reject_or_throw(request.body().type_);
    if !reject
        && request.body().family == AddressFamily::INet6
        && parsed
            .gateway
            .is_some_and(|gateway| gateway.is_unspecified())
    {
        return Err(SystemError::EINVAL);
    }
    let gateway = if reject {
        None
    } else {
        normalize_ip(parsed.gateway)
    };
    let requested_oif = parsed.oif.filter(|oif| *oif != 0);
    let oif = match gateway {
        Some(gateway) => route::resolve_gateway_oif(
            &netns,
            gateway,
            table,
            requested_oif,
            onlink,
            request.body().scope,
        )?,
        None if reject => {
            if let Some(oif) = requested_oif {
                let devices = netns.device_list();
                let iface = devices.get(&(oif as usize)).ok_or(SystemError::ENODEV)?;
                if onlink && !iface.flags().contains(InterfaceFlags::UP) {
                    return Err(SystemError::ENETDOWN);
                }
            } else if onlink {
                return Err(SystemError::EINVAL);
            }
            crate::net::LOOPBACK_IFINDEX as u32
        }
        None if request.body().family == AddressFamily::INet6
            && request.body().type_ == route::RTN_LOCAL =>
        {
            requested_oif.ok_or(SystemError::ENODEV)?
        }
        None => requested_oif.ok_or(SystemError::EINVAL)?,
    };
    let entry = RouteEntry {
        destination: route::canonical_cidr(parsed.destination),
        source: parsed.source,
        preferred_source: normalize_ip(parsed.preferred_source),
        table,
        priority: if request.body().family == AddressFamily::INet6 {
            parsed
                .priority
                .filter(|priority| *priority != 0)
                .unwrap_or(IP6_RT_PRIO_USER)
        } else {
            parsed.priority.unwrap_or(0)
        },
        tos: request.body().tos,
        protocol: if request.body().protocol == RouteProtocol::Unspec as u8 {
            RTPROT_BOOT
        } else {
            request.body().protocol
        },
        scope: if request.body().family == AddressFamily::INet6 {
            RouteScope::Universe as u8
        } else {
            request.body().scope
        },
        kind: if request.body().type_ == RouteType::Unspec as u8 {
            RTN_UNICAST
        } else {
            request.body().type_
        },
        oif,
        gateway,
        nexthop_flags: if onlink {
            RouteFlags::ONLINK.bits() as u8
        } else {
            0
        },
        address_local: false,
        ra_route: None,
    };
    let nl_flags = NewRequestFlags::from_bits_truncate(request.header().flags);
    let outcome = route::add_route(
        rtnl,
        &netns,
        entry,
        RouteNewFlags {
            replace: nl_flags.contains(NewRequestFlags::REPLACE),
            excl: nl_flags.contains(NewRequestFlags::EXCL),
            create: nl_flags.contains(NewRequestFlags::CREATE),
            append: nl_flags.contains(NewRequestFlags::APPEND),
        },
    )?;
    match outcome {
        RouteMutationOutcome::Added {
            route: added,
            appended,
        } => {
            let mut notification_flags = NewRequestFlags::CREATE;
            if appended {
                notification_flags.insert(NewRequestFlags::APPEND);
            }
            notify_route_with_flags(
                &netns,
                CSegmentType::NEWROUTE,
                added,
                notification_flags.bits(),
            );
        }
        RouteMutationOutcome::Replaced { new, .. } => notify_route_with_flags(
            &netns,
            CSegmentType::NEWROUTE,
            new,
            NewRequestFlags::REPLACE.bits(),
        ),
        RouteMutationOutcome::Unchanged(_) => {}
    }
    Ok(Vec::new())
}

pub(super) fn do_del_route(
    rtnl: &RtnlGuard,
    request: &RouteSegment,
    netns: Arc<NetNamespace>,
) -> Result<Vec<RouteNlSegment>, SystemError> {
    validate_delete_request(request)?;
    let parsed = ParsedRouteRequest::from_segment(request)?;
    let table = effective_route_table(request.body().table, parsed.table);
    let ipv4 = request.body().family == AddressFamily::INet;
    let gateway = normalize_ip(parsed.gateway);
    let removed = route::delete_route(
        rtnl,
        &netns,
        RouteDeleteSelector {
            destination: route::canonical_cidr(parsed.destination),
            source: parsed.source,
            table,
            priority: parsed.priority.filter(|priority| *priority != 0),
            tos: ipv4.then_some(request.body().tos).filter(|tos| *tos != 0),
            protocol: (request.body().protocol != RouteProtocol::Unspec as u8)
                .then_some(request.body().protocol),
            scope: (ipv4 && request.body().scope != RouteScope::Nowhere as u8)
                .then_some(request.body().scope),
            kind: (ipv4 && request.body().type_ != RouteType::Unspec as u8)
                .then_some(request.body().type_),
            oif: parsed.oif.filter(|oif| *oif != 0),
            gateway_specified: gateway.is_some() || (!ipv4 && parsed.gateway.is_some()),
            gateway,
            preferred_source: ipv4
                .then(|| normalize_ip(parsed.preferred_source))
                .flatten(),
        },
    )?;
    notify_route(&netns, CSegmentType::DELROUTE, removed);
    Ok(Vec::new())
}

#[derive(Debug, Clone, Copy)]
struct ParsedRouteRequest {
    destination: IpCidr,
    source: Option<IpCidr>,
    preferred_source: Option<IpAddress>,
    gateway: Option<IpAddress>,
    oif: Option<u32>,
    iif: Option<u32>,
    priority: Option<u32>,
    table: Option<u32>,
}

impl ParsedRouteRequest {
    fn from_segment(segment: &RouteSegment) -> Result<Self, SystemError> {
        let family = segment.body().family;
        let mut parsed = Self {
            destination: default_cidr(family, segment.body().dst_len)?,
            source: None,
            preferred_source: None,
            gateway: None,
            oif: None,
            iif: None,
            priority: None,
            table: None,
        };
        for attr in segment.attrs() {
            match attr {
                RouteAttr::Dst(bytes) => {
                    parsed.destination = parse_cidr(bytes, segment.body().dst_len, family)?;
                }
                RouteAttr::Src(bytes) => {
                    parsed.source = Some(parse_cidr(bytes, segment.body().src_len, family)?);
                }
                RouteAttr::Prefsrc(bytes) => {
                    parsed.preferred_source = Some(parse_ip(bytes, family)?);
                }
                RouteAttr::Gateway(bytes) => parsed.gateway = Some(parse_ip(bytes, family)?),
                RouteAttr::Oif(index) => parsed.oif = Some(*index),
                RouteAttr::Priority(metric) => parsed.priority = Some(*metric),
                RouteAttr::Table(table) => parsed.table = Some(*table),
                RouteAttr::Iif(index) => parsed.iif = Some(*index),
                RouteAttr::Metrics
                | RouteAttr::Multipath
                | RouteAttr::CacheInfo(_)
                | RouteAttr::Preference(_) => {}
            }
        }
        if family == AddressFamily::INet6 {
            // Linux identifies an IPv6 route by its source prefix, with /0
            // representing the destination-only route. An omitted attribute
            // leaves the address zero even when the header specifies a prefix.
            parsed.source = if segment.body().src_len == 0 {
                None
            } else {
                let source = match parsed.source {
                    Some(source) => source,
                    None => default_cidr(family, segment.body().src_len)?,
                };
                Some(route::canonical_cidr(source))
            };
        }
        Ok(parsed)
    }
}

fn validate_new_request(request: &RouteSegment) -> Result<(), SystemError> {
    reject_unsupported_mutation_attrs(request)?;
    validate_mutation_family(request.body().family)?;
    validate_mutation_tos(request.body().family, request.body().tos)?;
    if request.body().raw_flags & !RouteFlags::ONLINK.bits() != 0 {
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }
    if request.body().type_ > RTN_MAX {
        return Err(SystemError::EINVAL);
    }
    let kind = request.body().type_;
    let ipv6 = request.body().family == AddressFamily::INet6;
    let reject = ipv6 && route::is_ipv6_reject_or_throw(kind);
    let supported = matches!(kind, route::RTN_UNSPEC | RTN_UNICAST)
        || (ipv6 && route::is_ipv6_configurable_kind(kind));
    if !supported {
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }
    let gateway = request.attrs().iter().find_map(|attr| match attr {
        RouteAttr::Gateway(bytes) => parse_ip(bytes, request.body().family).ok(),
        _ => None,
    });
    if request.body().family == AddressFamily::INet
        && (request.body().scope > RouteScope::Host as u8
            || request.body().scope == RouteScope::Host as u8 && normalize_ip(gateway).is_some())
    {
        return Err(SystemError::EINVAL);
    }
    // Linux's IPv4 fib_create_info does not consume RTA_SRC selectors; the
    // attribute is ignored and the route is still created. Match that
    // behaviour instead of failing with EOPNOTSUPP.
    if request
        .attrs()
        .iter()
        .any(|attr| matches!(attr, RouteAttr::Iif(_)))
    {
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }
    validate_prefix_lengths(request)?;

    if !reject && request.body().flags.contains(RouteFlags::ONLINK) {
        let oif = request.attrs().iter().find_map(|attr| match attr {
            RouteAttr::Oif(oif) => Some(*oif),
            _ => None,
        });
        if normalize_ip(gateway).is_none() || oif.is_none_or(|oif| oif == 0) {
            return Err(SystemError::EINVAL);
        }
    }
    Ok(())
}

fn validate_delete_request(request: &RouteSegment) -> Result<(), SystemError> {
    reject_unsupported_mutation_attrs(request)?;
    validate_mutation_family(request.body().family)?;
    validate_mutation_tos(request.body().family, request.body().tos)?;
    if request.body().raw_flags != 0 {
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }
    if request.body().type_ > RTN_MAX {
        return Err(SystemError::EINVAL);
    }
    if request
        .attrs()
        .iter()
        .any(|attr| matches!(attr, RouteAttr::Iif(_)))
    {
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }
    validate_prefix_lengths(request)
}

fn reject_unsupported_mutation_attrs(request: &RouteSegment) -> Result<(), SystemError> {
    if request
        .attrs()
        .iter()
        .any(|attr| matches!(attr, RouteAttr::Metrics | RouteAttr::Multipath))
    {
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }
    Ok(())
}

fn validate_get_request(request: &RouteSegment) -> Result<(), SystemError> {
    validate_mutation_family(request.body().family)?;
    validate_lookup_tos(request.body().family, request.body().tos)?;
    let host_len = match request.body().family {
        AddressFamily::INet => 32,
        AddressFamily::INet6 => 128,
        _ => unreachable!(),
    };
    if (request.body().src_len != 0 && request.body().src_len != host_len)
        || (request.body().dst_len != 0 && request.body().dst_len != host_len)
        || request.body().table != RouteTable::Unspec as u8
        || request.body().protocol != RouteProtocol::Unspec as u8
        || request.body().scope != RouteScope::Universe as u8
        || request.body().type_ != RouteType::Unspec as u8
    {
        return Err(SystemError::EINVAL);
    }
    let has_src = request
        .attrs()
        .iter()
        .any(|attr| matches!(attr, RouteAttr::Src(_)));
    let has_dst = request
        .attrs()
        .iter()
        .any(|attr| matches!(attr, RouteAttr::Dst(_)));
    if has_src != (request.body().src_len != 0) || has_dst != (request.body().dst_len != 0) {
        return Err(SystemError::EINVAL);
    }
    if request.attrs().iter().any(|attr| {
        matches!(
            attr,
            RouteAttr::Gateway(_)
                | RouteAttr::Prefsrc(_)
                | RouteAttr::Priority(_)
                | RouteAttr::Table(_)
        )
    }) {
        return Err(SystemError::EINVAL);
    }

    let supported_flags = if request.body().family == AddressFamily::INet6 {
        // Linux ignores LOOKUP_TABLE on non-strict IPv6 requests.
        RouteFlags::LOOKUP_TABLE | RouteFlags::FIB_MATCH
    } else {
        if request.body().flags.contains(RouteFlags::FIB_MATCH) {
            return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
        }
        RouteFlags::LOOKUP_TABLE
    };
    if request.body().raw_flags & !supported_flags.bits() != 0 {
        return Err(SystemError::EINVAL);
    }
    Ok(())
}

fn validate_mutation_family(family: AddressFamily) -> Result<(), SystemError> {
    match family {
        AddressFamily::INet | AddressFamily::INet6 => Ok(()),
        _ => Err(SystemError::EAFNOSUPPORT),
    }
}

fn validate_mutation_tos(family: AddressFamily, tos: u8) -> Result<(), SystemError> {
    if tos == 0 {
        return Ok(());
    }
    match family {
        AddressFamily::INet if tos & 0x03 != 0 => Err(SystemError::EINVAL),
        AddressFamily::INet => Err(SystemError::EOPNOTSUPP_OR_ENOTSUP),
        AddressFamily::INet6 => Err(SystemError::EINVAL),
        _ => Err(SystemError::EAFNOSUPPORT),
    }
}

fn validate_lookup_tos(family: AddressFamily, tos: u8) -> Result<(), SystemError> {
    if tos == 0 {
        return Ok(());
    }
    match family {
        AddressFamily::INet | AddressFamily::INet6 => Err(SystemError::EOPNOTSUPP_OR_ENOTSUP),
        AddressFamily::Unspecified => Err(SystemError::EOPNOTSUPP_OR_ENOTSUP),
        _ => Err(SystemError::EAFNOSUPPORT),
    }
}

fn validate_prefix_lengths(request: &RouteSegment) -> Result<(), SystemError> {
    let max = match request.body().family {
        AddressFamily::INet => 32,
        AddressFamily::INet6 => 128,
        _ => return Err(SystemError::EAFNOSUPPORT),
    };
    if request.body().dst_len > max || request.body().src_len > max {
        return Err(SystemError::EINVAL);
    }
    Ok(())
}

fn lookup_route(
    request: &RouteSegment,
    netns: Arc<NetNamespace>,
) -> Result<Vec<RouteNlSegment>, SystemError> {
    validate_get_request(request)?;
    let parsed = ParsedRouteRequest::from_segment(request)?;
    let destination = parsed.destination.address();
    let requested_oif = parsed.oif.filter(|oif| *oif != 0);
    if parsed.iif.is_some_and(|iif| iif != 0) {
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }
    if let (IpAddress::Ipv4(_), Some(oif)) = (destination, requested_oif) {
        ensure_iface_up(&netns, oif)?;
    }
    let requested_source = parsed
        .source
        .map(|source| source.address())
        .filter(|source| !source.is_unspecified());
    if requested_source.is_some_and(|source| !source.is_unicast()) {
        return Err(SystemError::EINVAL);
    }
    let (decision, selected_source) = if matches!(destination, IpAddress::Ipv4(_)) {
        if requested_source.is_some_and(|source| {
            !netns.device_list().values().any(|candidate| {
                candidate
                    .common()
                    .ip_addrs()
                    .iter()
                    .any(|cidr| cidr.address() == source)
            })
        }) {
            return Err(SystemError::ENETUNREACH);
        }
        let resolved =
            route::resolve_ipv4_route(&netns, destination, requested_oif, requested_source)?;
        let selected = requested_source
            .is_none()
            .then_some(resolved.source)
            .filter(|source| !source.is_unspecified());
        (resolved.decision, selected)
    } else {
        let IpAddress::Ipv6(destination_v6) = destination else {
            unreachable!()
        };
        let source = match requested_source {
            Some(IpAddress::Ipv6(source)) => Some(source),
            _ => None,
        };
        let decision = route::lookup_ipv6_output(&netns, destination_v6, requested_oif, source);
        if let Some(rejection) =
            decision.and_then(|decision| route::ipv6_route_rejection(decision.matched.kind))
        {
            return Err(rejection.error);
        }
        if let Some(oif) = requested_oif {
            ensure_iface_up(&netns, oif)?;
        }
        let decision = decision.ok_or(SystemError::ENETUNREACH)?;
        let iface = netns
            .device_list()
            .get(&(decision.oif as usize))
            .cloned()
            .ok_or(SystemError::ENODEV)?;
        if request.body().flags.contains(RouteFlags::FIB_MATCH) {
            return Ok(vec![RouteNlSegment::NewRoute(route_to_segment(
                request.header(),
                CSegmentType::NEWROUTE,
                decision.matched,
                RouteReply::FibMatch,
                0,
            )?)]);
        }
        let selected = if requested_source.is_some() {
            None
        } else {
            decision.source.preferred().or_else(|| {
                crate::net::socket::inet::common::pick_configured_source_addr(&iface, &destination)
            })
        };
        (decision, selected)
    };
    let host = host_cidr(destination);
    let output = RouteEntry {
        destination: host,
        source: requested_source.map(host_cidr),
        preferred_source: selected_source,
        table: decision.table,
        oif: if decision.matched.is_explicit_ipv6_local() {
            crate::net::LOOPBACK_IFINDEX as u32
        } else {
            decision.matched.oif
        },
        protocol: if decision.matched.ra_route.is_some()
            || decision.matched.is_explicit_ipv6_local()
        {
            decision.matched.protocol
        } else {
            RouteProtocol::Unspec as u8
        },
        scope: RouteScope::Universe as u8,
        ..decision.matched
    };
    Ok(vec![RouteNlSegment::NewRoute(route_to_segment(
        request.header(),
        CSegmentType::NEWROUTE,
        output,
        RouteReply::Resolved,
        0,
    )?)])
}

pub(super) fn notify_route(netns: &Arc<NetNamespace>, kind: CSegmentType, route: RouteEntry) {
    let flags = if kind == CSegmentType::NEWROUTE {
        (NewRequestFlags::CREATE | NewRequestFlags::APPEND).bits()
    } else {
        0
    };
    notify_route_with_flags(netns, kind, route, flags)
}

pub(crate) fn notify_ra_route(
    netns: &Arc<NetNamespace>,
    route: RouteEntry,
    removed: bool,
    replaced: bool,
) {
    let (kind, flags) = if removed {
        (CSegmentType::DELROUTE, 0)
    } else if replaced {
        (CSegmentType::NEWROUTE, NewRequestFlags::REPLACE.bits())
    } else {
        (
            CSegmentType::NEWROUTE,
            (NewRequestFlags::CREATE | NewRequestFlags::EXCL).bits(),
        )
    };
    notify_route_with_flags(netns, kind, route, flags);
}

fn notify_route_with_flags(
    netns: &Arc<NetNamespace>,
    kind: CSegmentType,
    route: RouteEntry,
    nlmsg_flags: u16,
) {
    let segment = match route_to_segment(
        &kernel_notify_header(kind),
        kind,
        route,
        RouteReply::Configuration,
        nlmsg_flags,
    ) {
        Ok(segment) => segment,
        Err(error) => {
            // Route publication has already committed. Linux treats rtnetlink
            // multicast allocation failure as a notification loss, not as a
            // failed mutation that callers should retry.
            log::warn!("failed to serialize route notification: {:?}", error);
            return;
        }
    };
    multicast_notify(
        netns.clone(),
        route_notify_group(route.destination.address()),
        match kind {
            CSegmentType::NEWROUTE => RouteNlSegment::NewRoute(segment),
            CSegmentType::DELROUTE => RouteNlSegment::DelRoute(segment),
            _ => unreachable!(),
        },
    );
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum RouteReply {
    Configuration,
    Resolved,
    FibMatch,
}

pub(super) fn route_to_segment(
    request_header: &CMsgSegHdr,
    msg_type: CSegmentType,
    route: RouteEntry,
    reply: RouteReply,
    nlmsg_flags: u16,
) -> Result<RouteSegment, SystemError> {
    let body_flags = match reply {
        RouteReply::Resolved => RouteFlags::CLONED,
        RouteReply::Configuration | RouteReply::FibMatch => {
            RouteFlags::from_bits_truncate(route.nexthop_flags as u32)
        }
    };
    let body = RouteSegmentBody {
        family: family_of_ip(route.destination.address()),
        dst_len: route.destination.prefix_len(),
        src_len: route.source.map_or(0, |source| source.prefix_len()),
        tos: route.tos,
        table: if route.table <= u8::MAX as u32 {
            route.table as u8
        } else {
            RouteTable::Compat as u8
        },
        protocol: route.protocol,
        scope: if reply == RouteReply::FibMatch {
            RouteScope::Universe as u8
        } else {
            route.scope
        },
        type_: route.kind,
        raw_flags: body_flags.bits(),
        flags: body_flags,
    };
    let header = CMsgSegHdr {
        len: 0,
        type_: msg_type as u16,
        flags: nlmsg_flags,
        seq: request_header.seq,
        pid: request_header.pid,
    };
    let mut attrs = FallibleRouteAttrs::new();
    if route.destination.prefix_len() != 0 {
        attrs.push(RouteAttr::Dst(ip_to_bytes(route.destination.address())?))?;
    }
    attrs.push(RouteAttr::Oif(route.oif))?;
    attrs.push(RouteAttr::Table(route.table))?;
    if let Some(source) = route.source {
        attrs.push(RouteAttr::Src(ip_to_bytes(source.address())?))?;
    }
    if let Some(source) = route.preferred_source {
        attrs.push(RouteAttr::Prefsrc(ip_to_bytes(source)?))?;
    }
    if let Some(gateway) = route.gateway {
        attrs.push(RouteAttr::Gateway(ip_to_bytes(gateway)?))?;
    }
    if reply == RouteReply::FibMatch
        || (route.priority != 0
            && (reply != RouteReply::Resolved
                || matches!(route.destination.address(), IpAddress::Ipv6(_))))
    {
        attrs.push(RouteAttr::Priority(route.priority))?;
    }
    let deadline = route.expires_at_us();
    if deadline.is_some() || reply == RouteReply::FibMatch {
        let mut cache_info = [0; 32];
        if let Some(deadline) = deadline {
            // Linux rta_cacheinfo stores signed remaining time in USER_HZ ticks.
            let now = crate::time::Instant::now().total_micros();
            let ticks = deadline.saturating_sub(now) / 10_000;
            let expires = ticks.clamp(-(i32::MAX as i64), i32::MAX as i64) as i32;
            cache_info[8..12].copy_from_slice(&expires.to_ne_bytes());
        }
        attrs.push(RouteAttr::CacheInfo(cache_info))?;
    }
    let preference = match route.learned_default().map(|route| route.preference) {
        Some(route::RouterPreference::High) => Some(1),
        Some(route::RouterPreference::Medium) => Some(0),
        Some(route::RouterPreference::Low) => Some(3),
        None if reply == RouteReply::FibMatch => Some(0),
        None => None,
    };
    if let Some(preference) = preference {
        attrs.push(RouteAttr::Preference(preference))?;
    }
    Ok(RouteSegment::new(header, body, attrs.into_inner()))
}

/// Route attribute builder whose only append operation is allocation-aware.
struct FallibleRouteAttrs(Vec<RouteAttr>);

impl FallibleRouteAttrs {
    fn new() -> Self {
        Self(Vec::new())
    }

    fn push(&mut self, attr: RouteAttr) -> Result<(), SystemError> {
        self.0.try_reserve(1).map_err(|_| SystemError::ENOMEM)?;
        self.0.push(attr);
        Ok(())
    }

    fn into_inner(self) -> Vec<RouteAttr> {
        self.0
    }
}

fn family_of_ip(ip: IpAddress) -> AddressFamily {
    match ip {
        IpAddress::Ipv4(_) => AddressFamily::INet,
        IpAddress::Ipv6(_) => AddressFamily::INet6,
    }
}

fn default_cidr(family: AddressFamily, prefix_len: u8) -> Result<IpCidr, SystemError> {
    match family {
        AddressFamily::INet if prefix_len <= 32 => Ok(IpCidr::Ipv4(Ipv4Cidr::new(
            Ipv4Address::UNSPECIFIED,
            prefix_len,
        ))),
        AddressFamily::INet6 if prefix_len <= 128 => Ok(IpCidr::Ipv6(Ipv6Cidr::new(
            Ipv6Address::UNSPECIFIED,
            prefix_len,
        ))),
        _ => Err(SystemError::EAFNOSUPPORT),
    }
}

fn host_cidr(address: IpAddress) -> IpCidr {
    match address {
        IpAddress::Ipv4(address) => IpCidr::Ipv4(Ipv4Cidr::new(address, 32)),
        IpAddress::Ipv6(address) => IpCidr::Ipv6(Ipv6Cidr::new(address, 128)),
    }
}

fn parse_cidr(bytes: &[u8], prefix_len: u8, family: AddressFamily) -> Result<IpCidr, SystemError> {
    let cidr = match family {
        AddressFamily::INet if prefix_len <= 32 && bytes.len() == 4 => IpCidr::Ipv4(Ipv4Cidr::new(
            Ipv4Address::from_octets(bytes.try_into().map_err(|_| SystemError::EINVAL)?),
            prefix_len,
        )),
        AddressFamily::INet6
            if prefix_len <= 128
                && bytes.len() >= usize::from(prefix_len).div_ceil(8)
                && bytes.len() <= 16 =>
        {
            let mut octets = [0; 16];
            octets[..bytes.len()].copy_from_slice(bytes);
            IpCidr::Ipv6(Ipv6Cidr::new(Ipv6Address::from_octets(octets), prefix_len))
        }
        _ => return Err(SystemError::EINVAL),
    };
    if matches!(cidr, IpCidr::Ipv4(_)) && route::canonical_cidr(cidr) != cidr {
        return Err(SystemError::EINVAL);
    }
    Ok(cidr)
}

fn parse_ip(bytes: &[u8], family: AddressFamily) -> Result<IpAddress, SystemError> {
    match family {
        AddressFamily::INet if bytes.len() == 4 => Ok(IpAddress::Ipv4(Ipv4Address::from_octets(
            bytes.try_into().map_err(|_| SystemError::EINVAL)?,
        ))),
        AddressFamily::INet6 if bytes.len() == 16 => Ok(IpAddress::Ipv6(Ipv6Address::from_octets(
            bytes.try_into().map_err(|_| SystemError::EINVAL)?,
        ))),
        _ => Err(SystemError::EINVAL),
    }
}

fn ip_to_bytes(ip: IpAddress) -> Result<Vec<u8>, SystemError> {
    let len = match ip {
        IpAddress::Ipv4(_) => 4,
        IpAddress::Ipv6(_) => 16,
    };
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(len)
        .map_err(|_| SystemError::ENOMEM)?;
    match ip {
        IpAddress::Ipv4(address) => bytes.extend_from_slice(&address.octets()),
        IpAddress::Ipv6(address) => bytes.extend_from_slice(&address.octets()),
    }
    Ok(bytes)
}

fn normalize_ip(ip: Option<IpAddress>) -> Option<IpAddress> {
    ip.filter(|address| !address.is_unspecified())
}

fn ensure_iface_up(netns: &Arc<NetNamespace>, oif: u32) -> Result<(), SystemError> {
    let iface = netns
        .device_list()
        .get(&(oif as usize))
        .cloned()
        .ok_or(SystemError::ENODEV)?;
    if !iface.flags().contains(InterfaceFlags::UP) {
        return Err(SystemError::ENETUNREACH);
    }
    Ok(())
}

fn route_notify_group(ip: IpAddress) -> u32 {
    match ip {
        IpAddress::Ipv4(_) => RTMGRP_IPV4_ROUTE,
        IpAddress::Ipv6(_) => RTMGRP_IPV6_ROUTE,
    }
}
