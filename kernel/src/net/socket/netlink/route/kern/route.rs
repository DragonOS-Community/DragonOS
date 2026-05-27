use crate::{
    driver::net::{Iface, NetlinkRouteEntry},
    net::socket::{
        netlink::{
            message::segment::{
                header::{CMsgSegHdr, GetRequestFlags, NewRequestFlags, SegHdrCommonFlags},
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
                            RouteFlags, RouteProtocol, RouteScope, RouteSegment, RouteSegmentBody,
                            RouteTable, RouteType,
                        },
                        RouteNlSegment,
                    },
                },
            },
        },
        AddressFamily,
    },
    process::namespace::net_namespace::NetNamespace,
};
use alloc::sync::Arc;
use alloc::vec::Vec;
use smoltcp::wire::{IpAddress, IpCidr, Ipv4Address, Ipv4Cidr, Ipv6Address, Ipv6Cidr};
use system_error::SystemError;

/// `rtm_table == 0` 表示 RT_TABLE_MAIN（254），与 Linux 一致。
fn effective_route_table(table: u8) -> u8 {
    if table == 0 {
        RouteTable::Main as u8
    } else {
        table
    }
}

fn route_entry_key(entry: &NetlinkRouteEntry) -> (IpCidr, u8, Option<(IpAddress, u8)>) {
    (
        entry.destination,
        entry.table,
        entry
            .source
            .as_ref()
            .map(|cidr| (cidr.address(), cidr.prefix_len())),
    )
}

fn netlink_route_exists(iface: &Arc<dyn Iface>, entry: &NetlinkRouteEntry) -> bool {
    let key = route_entry_key(entry);
    iface
        .common()
        .netlink_routes()
        .iter()
        .any(|existing| route_entry_key(existing) == key)
}

pub(super) fn do_get_route(
    request_segment: &RouteSegment,
    netns: Arc<NetNamespace>,
) -> Result<Vec<RouteNlSegment>, SystemError> {
    let dump_all = GetRequestFlags::from_bits_truncate(request_segment.header().flags)
        .contains(GetRequestFlags::DUMP);
    if !dump_all {
        if request_segment
            .body()
            .flags
            .contains(RouteFlags::LOOKUP_TABLE)
        {
            return do_lookup_route(request_segment, netns);
        }
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }

    let requested_family = request_segment.body().family;
    let mut response = Vec::new();

    for (_, iface) in netns.device_list().iter() {
        response.extend(build_connected_route_segments(
            request_segment,
            iface,
            requested_family,
        )?);
        response.extend(build_netlink_route_segments(
            request_segment,
            iface,
            requested_family,
        )?);
    }

    finish_response(request_segment.header(), true, &mut response);
    Ok(response)
}

/// RTM_GETRULE：当前仅支持 DUMP 语义，返回空规则表 + NLMSG_DONE。
pub(super) fn do_get_rule(
    request_segment: &RouteSegment,
    _netns: Arc<NetNamespace>,
) -> Result<Vec<RouteNlSegment>, SystemError> {
    let dump_all = GetRequestFlags::from_bits_truncate(request_segment.header().flags)
        .contains(GetRequestFlags::DUMP);
    if !dump_all {
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }
    let mut response = Vec::new();
    finish_response(request_segment.header(), true, &mut response);
    Ok(response)
}

pub(super) fn do_new_route(
    request_segment: &RouteSegment,
    netns: Arc<NetNamespace>,
) -> Result<Vec<RouteNlSegment>, SystemError> {
    let parsed = ParsedRouteRequest::from_segment(request_segment)?;
    let iface = netns
        .device_list()
        .get(&(parsed.oif as usize))
        .cloned()
        .ok_or(SystemError::ENODEV)?;

    let table = effective_route_table(parsed.table);
    let entry = NetlinkRouteEntry {
        destination: parsed.destination,
        source: parsed.source,
        gateway: parsed.gateway,
        priority: parsed.priority,
        table,
        protocol: request_segment.body().protocol as u8,
        scope: request_segment.body().scope as u8,
        kind: request_segment.body().type_ as u8,
    };

    let replace = NewRequestFlags::from_bits_truncate(request_segment.header().flags)
        .contains(NewRequestFlags::REPLACE);
    if netlink_route_exists(&iface, &entry) && !replace {
        return Err(SystemError::EEXIST);
    }

    iface.common().upsert_netlink_route(entry);

    if let Err(err) = sync_iface_route_table(&iface, &parsed) {
        iface
            .common()
            .remove_netlink_route(parsed.destination, parsed.source, table);
        return Err(err);
    }
    multicast_notify(
        netns,
        route_notify_group(parsed.destination.address()),
        RouteNlSegment::NewRoute(route_to_segment(
            &kernel_notify_header(CSegmentType::NEWROUTE),
            CSegmentType::NEWROUTE,
            &iface,
            RouteView {
                destination: parsed.destination,
                source: parsed.source,
                gateway: parsed.gateway,
                priority: parsed.priority,
                table,
                protocol: request_segment.body().protocol as u8,
                scope: request_segment.body().scope as u8,
                kind: request_segment.body().type_ as u8,
                flags: RouteFlags::empty(),
            },
        )?),
    );
    Ok(Vec::new())
}

pub(super) fn do_del_route(
    request_segment: &RouteSegment,
    netns: Arc<NetNamespace>,
) -> Result<Vec<RouteNlSegment>, SystemError> {
    let parsed = ParsedRouteRequest::from_segment(request_segment)?;
    let iface = netns
        .device_list()
        .get(&(parsed.oif as usize))
        .cloned()
        .ok_or(SystemError::ENODEV)?;

    let table = effective_route_table(parsed.table);
    if !iface
        .common()
        .remove_netlink_route(parsed.destination, parsed.source, table)
    {
        return Err(SystemError::ESRCH);
    }

    sync_iface_route_table_remove(
        &iface,
        parsed.destination,
        parsed.source,
        parsed.gateway,
        table,
    );
    multicast_notify(
        netns,
        route_notify_group(parsed.destination.address()),
        RouteNlSegment::DelRoute(route_to_segment(
            &kernel_notify_header(CSegmentType::DELROUTE),
            CSegmentType::DELROUTE,
            &iface,
            RouteView {
                destination: parsed.destination,
                source: parsed.source,
                gateway: parsed.gateway,
                priority: parsed.priority,
                table,
                protocol: request_segment.body().protocol as u8,
                scope: request_segment.body().scope as u8,
                kind: request_segment.body().type_ as u8,
                flags: RouteFlags::empty(),
            },
        )?),
    );
    Ok(Vec::new())
}

#[derive(Debug, Clone, Copy)]
struct ParsedRouteRequest {
    destination: IpCidr,
    source: Option<IpCidr>,
    gateway: Option<IpAddress>,
    oif: u32,
    priority: u32,
    table: u8,
}

impl ParsedRouteRequest {
    fn from_segment(segment: &RouteSegment) -> Result<Self, SystemError> {
        let family = segment.body().family;
        let mut destination = default_cidr(family)?;
        let mut source = None;
        let mut gateway = None;
        let mut oif = None;
        let mut priority = 0;
        let mut table = segment.body().table as u8;

        for attr in segment.attrs() {
            match attr {
                RouteAttr::Dst(bytes) => {
                    destination = parse_cidr(bytes, segment.body().dst_len, family)?;
                }
                RouteAttr::Src(bytes) => {
                    source = Some(parse_cidr(bytes, segment.body().src_len, family)?);
                }
                RouteAttr::Prefsrc(bytes) => {
                    source = Some(parse_cidr(bytes, 0, family)?);
                }
                RouteAttr::Gateway(bytes) => gateway = Some(parse_ip(bytes, family)?),
                RouteAttr::Oif(index) => oif = Some(*index),
                RouteAttr::Priority(metric) => priority = *metric,
                RouteAttr::Table(route_table) => table = *route_table as u8,
                RouteAttr::Iif(_) => {}
            }
        }

        let oif = oif.ok_or(SystemError::EINVAL)?;
        Ok(Self {
            destination,
            source,
            gateway,
            oif,
            priority,
            table,
        })
    }
}

fn build_connected_route_segments(
    request_segment: &RouteSegment,
    iface: &Arc<dyn Iface>,
    requested_family: AddressFamily,
) -> Result<Vec<RouteNlSegment>, SystemError> {
    let mut segments = Vec::new();
    for cidr in iface.common().ip_addrs().iter() {
        let family = family_of_ip(cidr.address());
        if !family_matches(requested_family, family) {
            continue;
        }

        let scope = if iface.name() == "lo" {
            RouteScope::Host
        } else {
            RouteScope::Link
        };

        segments.push(RouteNlSegment::NewRoute(route_to_segment(
            request_segment.header(),
            CSegmentType::NEWROUTE,
            iface,
            RouteView {
                destination: *cidr,
                source: None,
                gateway: None,
                priority: 0,
                table: RouteTable::Main as u8,
                protocol: RouteProtocol::Kernel as u8,
                scope: scope as u8,
                kind: RouteType::Unicast as u8,
                flags: RouteFlags::empty(),
            },
        )?));
    }

    Ok(segments)
}

fn build_netlink_route_segments(
    request_segment: &RouteSegment,
    iface: &Arc<dyn Iface>,
    requested_family: AddressFamily,
) -> Result<Vec<RouteNlSegment>, SystemError> {
    iface
        .common()
        .netlink_routes()
        .iter()
        .filter(|route| family_matches(requested_family, family_of_ip(route.destination.address())))
        .map(|route| {
            route_to_segment(
                request_segment.header(),
                CSegmentType::NEWROUTE,
                iface,
                RouteView {
                    destination: route.destination,
                    source: route.source,
                    gateway: route.gateway,
                    priority: route.priority,
                    table: route.table,
                    protocol: route.protocol,
                    scope: route.scope,
                    kind: route.kind,
                    flags: RouteFlags::empty(),
                },
            )
            .map(RouteNlSegment::NewRoute)
        })
        .collect()
}

fn do_lookup_route(
    request_segment: &RouteSegment,
    netns: Arc<NetNamespace>,
) -> Result<Vec<RouteNlSegment>, SystemError> {
    let family = request_segment.body().family;
    let mut destination = default_cidr(family)?;
    for attr in request_segment.attrs() {
        if let RouteAttr::Dst(bytes) = attr {
            destination = parse_cidr(bytes, request_segment.body().dst_len, family)?;
        }
    }

    let lo = netns.loopback_iface().ok_or(SystemError::ENODEV)?;
    let iface: Arc<dyn Iface> = lo;

    let segment = route_to_segment(
        request_segment.header(),
        CSegmentType::NEWROUTE,
        &iface,
        RouteView {
            destination,
            source: None,
            gateway: None,
            priority: 0,
            table: RouteTable::Main as u8,
            protocol: RouteProtocol::Kernel as u8,
            scope: RouteScope::Host as u8,
            kind: RouteType::Unicast as u8,
            flags: RouteFlags::CLONED,
        },
    )?;

    Ok(vec![RouteNlSegment::NewRoute(segment)])
}

#[derive(Debug, Clone, Copy)]
struct RouteView {
    destination: IpCidr,
    source: Option<IpCidr>,
    gateway: Option<IpAddress>,
    priority: u32,
    table: u8,
    protocol: u8,
    scope: u8,
    kind: u8,
    flags: RouteFlags,
}

fn route_to_segment(
    request_header: &CMsgSegHdr,
    msg_type: CSegmentType,
    iface: &Arc<dyn Iface>,
    route: RouteView,
) -> Result<crate::net::socket::netlink::route::message::segment::route::RouteSegment, SystemError>
{
    let family = family_of_ip(route.destination.address());
    let header = crate::net::socket::netlink::message::segment::header::CMsgSegHdr {
        len: 0,
        type_: msg_type as u16,
        flags: SegHdrCommonFlags::empty().bits(),
        seq: request_header.seq,
        pid: request_header.pid,
    };
    let body = RouteSegmentBody {
        family,
        dst_len: route.destination.prefix_len(),
        src_len: route.source.map(|cidr| cidr.prefix_len()).unwrap_or(0),
        tos: 0,
        table: RouteTable::try_from(route.table).unwrap_or(RouteTable::Main),
        protocol: RouteProtocol::try_from(route.protocol).unwrap_or(RouteProtocol::Boot),
        scope: RouteScope::try_from(route.scope).unwrap_or(RouteScope::Universe),
        type_: RouteType::try_from(route.kind).unwrap_or(RouteType::Unicast),
        flags: route.flags,
    };

    let mut attrs = vec![
        RouteAttr::Dst(ip_to_bytes(route_destination_prefix(route.destination))),
        RouteAttr::Oif(iface.nic_id() as u32),
    ];
    if let Some(source) = route.source {
        attrs.push(RouteAttr::Src(ip_to_bytes(source.address())));
    }
    if let Some(gateway) = route.gateway {
        attrs.push(RouteAttr::Gateway(ip_to_bytes(gateway)));
    }
    if route.priority != 0 {
        attrs.push(RouteAttr::Priority(route.priority));
    }

    Ok(RouteSegment::new(header, body, attrs))
}

fn sync_iface_route_table(
    iface: &Arc<dyn Iface>,
    route: &ParsedRouteRequest,
) -> Result<(), SystemError> {
    let mut push_failed = false;
    iface.smol_iface().lock().routes_mut().update(|routes| {
        routes.retain(|existing| {
            existing.cidr != route.destination || is_local_connected_route(iface, existing)
        });

        if routes
            .push(smoltcp::iface::Route {
                cidr: route.destination,
                via_router: route.gateway,
                preferred_until: None,
                expires_at: None,
            })
            .is_err()
        {
            push_failed = true;
        }
    });
    if push_failed {
        Err(SystemError::ENOSPC)
    } else {
        Ok(())
    }
}

fn sync_iface_route_table_remove(
    iface: &Arc<dyn Iface>,
    destination: IpCidr,
    source: Option<IpCidr>,
    gateway: Option<IpAddress>,
    table: u8,
) {
    iface.smol_iface().lock().routes_mut().update(|routes| {
        routes.retain(|existing| {
            if is_local_connected_route(iface, existing) {
                return true;
            }
            if existing.cidr != destination {
                return true;
            }
            existing.via_router != gateway
        });
    });
    let _ = (source, table);
}

fn family_matches(requested_family: AddressFamily, actual_family: AddressFamily) -> bool {
    requested_family == AddressFamily::Unspecified || requested_family == actual_family
}

fn family_of_ip(ip: IpAddress) -> AddressFamily {
    match ip {
        IpAddress::Ipv4(_) => AddressFamily::INet,
        IpAddress::Ipv6(_) => AddressFamily::INet6,
    }
}

fn default_cidr(family: AddressFamily) -> Result<IpCidr, SystemError> {
    match family {
        AddressFamily::INet => Ok(IpCidr::Ipv4(Ipv4Cidr::new(Ipv4Address::UNSPECIFIED, 0))),
        AddressFamily::INet6 => Ok(IpCidr::Ipv6(Ipv6Cidr::new(Ipv6Address::UNSPECIFIED, 0))),
        _ => Err(SystemError::EAFNOSUPPORT),
    }
}

fn parse_cidr(bytes: &[u8], prefix_len: u8, family: AddressFamily) -> Result<IpCidr, SystemError> {
    let ip = parse_ip(bytes, family)?;
    match ip {
        IpAddress::Ipv4(addr) if prefix_len <= 32 => {
            Ok(IpCidr::Ipv4(Ipv4Cidr::new(addr, prefix_len)))
        }
        IpAddress::Ipv6(addr) if prefix_len <= 128 => {
            Ok(IpCidr::Ipv6(Ipv6Cidr::new(addr, prefix_len)))
        }
        _ => Err(SystemError::EINVAL),
    }
}

fn parse_ip(bytes: &[u8], family: AddressFamily) -> Result<IpAddress, SystemError> {
    match family {
        AddressFamily::INet if bytes.len() == 4 => Ok(IpAddress::Ipv4(Ipv4Address::new(
            bytes[0], bytes[1], bytes[2], bytes[3],
        ))),
        AddressFamily::INet6 if bytes.len() == 16 => Ok(IpAddress::Ipv6(Ipv6Address::new(
            u16::from_be_bytes([bytes[0], bytes[1]]),
            u16::from_be_bytes([bytes[2], bytes[3]]),
            u16::from_be_bytes([bytes[4], bytes[5]]),
            u16::from_be_bytes([bytes[6], bytes[7]]),
            u16::from_be_bytes([bytes[8], bytes[9]]),
            u16::from_be_bytes([bytes[10], bytes[11]]),
            u16::from_be_bytes([bytes[12], bytes[13]]),
            u16::from_be_bytes([bytes[14], bytes[15]]),
        ))),
        _ => Err(SystemError::EINVAL),
    }
}

fn ip_to_bytes(ip: IpAddress) -> Vec<u8> {
    match ip {
        IpAddress::Ipv4(addr) => addr.octets().to_vec(),
        IpAddress::Ipv6(addr) => addr.octets().to_vec(),
    }
}

fn route_destination_prefix(cidr: IpCidr) -> IpAddress {
    match cidr {
        IpCidr::Ipv4(cidr) => IpAddress::Ipv4(cidr.network().address()),
        IpCidr::Ipv6(cidr) => {
            let mut octets = cidr.address().octets();
            let prefix_len = cidr.prefix_len() as usize;
            let full_bytes = prefix_len / 8;
            let partial_bits = prefix_len % 8;

            if partial_bits != 0 && full_bytes < octets.len() {
                octets[full_bytes] &= 0xffu8 << (8 - partial_bits);
            }
            for byte in octets
                .iter_mut()
                .skip(full_bytes + usize::from(partial_bits != 0))
            {
                *byte = 0;
            }

            IpAddress::Ipv6(Ipv6Address::from(octets))
        }
    }
}

fn is_local_connected_route(iface: &Arc<dyn Iface>, route: &smoltcp::iface::Route) -> bool {
    route.via_router.is_none() && iface.common().ip_addrs().contains(&route.cidr)
}

fn route_notify_group(ip: IpAddress) -> u32 {
    match ip {
        IpAddress::Ipv4(_) => RTMGRP_IPV4_ROUTE,
        IpAddress::Ipv6(_) => RTMGRP_IPV6_ROUTE,
    }
}
