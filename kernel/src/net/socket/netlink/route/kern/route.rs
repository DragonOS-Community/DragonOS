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
                    header::{CMsgSegHdr, GetRequestFlags, NewRequestFlags},
                    CSegmentType,
                },
                route::{
                    kern::utils::{
                        finish_response, kernel_notify_header, multicast_notify, RTMGRP_IPV4_ROUTE,
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
    if GetRequestFlags::from_bits_truncate(request.header().flags).contains(GetRequestFlags::DUMP) {
        return dump_routes(request, netns);
    }
    lookup_route(request, netns)
}

pub(super) fn do_get_rule(
    request: &RouteSegment,
    _netns: Arc<NetNamespace>,
) -> Result<Vec<RouteNlSegment>, SystemError> {
    if !GetRequestFlags::from_bits_truncate(request.header().flags).contains(GetRequestFlags::DUMP)
    {
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }
    let mut response = Vec::new();
    finish_response(request.header(), true, &mut response)?;
    Ok(response)
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
    if request.body().family == AddressFamily::INet6
        && parsed
            .gateway
            .is_some_and(|gateway| gateway.is_unspecified())
    {
        return Err(SystemError::EINVAL);
    }
    let gateway = normalize_ip(parsed.gateway);
    let oif = match gateway {
        Some(gateway) => route::resolve_gateway_oif(
            &netns,
            gateway,
            table,
            parsed.oif.filter(|oif| *oif != 0),
            onlink,
            request.body().scope,
        )?,
        None => parsed
            .oif
            .filter(|oif| *oif != 0)
            .ok_or(SystemError::EINVAL)?,
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
            }
        }
        Ok(parsed)
    }
}

fn validate_new_request(request: &RouteSegment) -> Result<(), SystemError> {
    validate_mutation_family(request.body().family)?;
    validate_mutation_tos(request.body().family, request.body().tos)?;
    if request.body().flags.bits() & !RouteFlags::ONLINK.bits() != 0 {
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }
    if request.body().type_ > RTN_MAX {
        return Err(SystemError::EINVAL);
    }
    if request.body().type_ != RouteType::Unspec as u8
        && request.body().type_ != RouteType::Unicast as u8
    {
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
    if request.body().src_len != 0
        || request
            .attrs()
            .iter()
            .any(|attr| matches!(attr, RouteAttr::Src(_) | RouteAttr::Iif(_)))
    {
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }
    validate_prefix_lengths(request)?;

    if request.body().flags.contains(RouteFlags::ONLINK) {
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
    validate_mutation_family(request.body().family)?;
    validate_mutation_tos(request.body().family, request.body().tos)?;
    if !request.body().flags.is_empty() {
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }
    if request.body().type_ > RTN_MAX {
        return Err(SystemError::EINVAL);
    }
    if request.body().src_len != 0
        || request
            .attrs()
            .iter()
            .any(|attr| matches!(attr, RouteAttr::Src(_) | RouteAttr::Iif(_)))
    {
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }
    validate_prefix_lengths(request)
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

    if request.body().flags.contains(RouteFlags::FIB_MATCH) {
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }
    match request.body().family {
        AddressFamily::INet => {
            if request.body().flags.bits() & !RouteFlags::LOOKUP_TABLE.bits() != 0 {
                return Err(SystemError::EINVAL);
            }
        }
        AddressFamily::INet6 if !request.body().flags.is_empty() => {
            return Err(SystemError::EINVAL)
        }
        AddressFamily::INet6 => {}
        _ => unreachable!(),
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

fn dump_routes(
    request: &RouteSegment,
    netns: Arc<NetNamespace>,
) -> Result<Vec<RouteNlSegment>, SystemError> {
    // Linux applies rtmsg header/attribute dump filters only when the socket
    // enabled NETLINK_GET_STRICT_CHK. DragonOS does not expose that opt-in yet,
    // so its default dump path follows Linux's lenient mode and ignores them.
    let family = request.body().family;
    let entries = route::snapshot(&netns)?;
    let route_count = entries
        .iter()
        .filter(|entry| family_matches(family, family_of_ip(entry.destination.address())))
        .count();
    let mut response = Vec::new();
    response
        .try_reserve(route_count.checked_add(1).ok_or(SystemError::ENOMEM)?)
        .map_err(|_| SystemError::ENOMEM)?;
    for entry in entries {
        if family_matches(family, family_of_ip(entry.destination.address())) {
            response.push(RouteNlSegment::NewRoute(route_to_segment(
                request.header(),
                CSegmentType::NEWROUTE,
                entry,
                RouteFlags::empty(),
                false,
                0,
            )?));
        }
    }
    finish_response(request.header(), true, &mut response)?;
    Ok(response)
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
    if let Some(oif) = requested_oif {
        ensure_iface_up(&netns, oif)?;
    }
    let decision = match requested_oif {
        Some(oif) => route::lookup_on_iface(&netns, destination, oif),
        None => route::lookup(&netns, destination),
    }
    .ok_or(SystemError::ENETUNREACH)?;
    let iface = netns
        .device_list()
        .get(&(decision.oif as usize))
        .cloned()
        .ok_or(SystemError::ENODEV)?;
    let requested_source = parsed
        .source
        .map(|source| source.address())
        .filter(|source| !source.is_unspecified());
    if requested_source.is_some_and(|source| !source.is_unicast()) {
        return Err(SystemError::EINVAL);
    }
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
    let selected_source = if requested_source.is_some() {
        None
    } else {
        decision.source.preferred().or_else(|| {
            crate::net::socket::inet::common::pick_configured_source_addr(&iface, &destination)
        })
    };
    let host = host_cidr(destination);
    let output = RouteEntry {
        destination: host,
        source: requested_source.map(host_cidr),
        preferred_source: selected_source,
        table: decision.table,
        protocol: RouteProtocol::Unspec as u8,
        scope: RouteScope::Universe as u8,
        ..decision.matched
    };
    Ok(vec![RouteNlSegment::NewRoute(route_to_segment(
        request.header(),
        CSegmentType::NEWROUTE,
        output,
        RouteFlags::CLONED,
        true,
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
        RouteFlags::empty(),
        false,
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

fn route_to_segment(
    request_header: &CMsgSegHdr,
    msg_type: CSegmentType,
    route: RouteEntry,
    flags: RouteFlags,
    output: bool,
    nlmsg_flags: u16,
) -> Result<RouteSegment, SystemError> {
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
        scope: route.scope,
        type_: route.kind,
        flags: if output {
            flags
        } else {
            flags | RouteFlags::from_bits_truncate(route.nexthop_flags as u32)
        },
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
    if route.priority != 0 && (!output || matches!(route.destination.address(), IpAddress::Ipv6(_)))
    {
        attrs.push(RouteAttr::Priority(route.priority))?;
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

fn family_matches(requested: AddressFamily, actual: AddressFamily) -> bool {
    requested == AddressFamily::Unspecified || requested == actual
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
