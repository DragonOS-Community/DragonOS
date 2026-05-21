use crate::{
    driver::net::Iface,
    net::socket::{
        netlink::{
            message::segment::{
                header::{CMsgSegHdr, GetRequestFlags, NewRequestFlags, SegHdrCommonFlags},
                CSegmentType,
            },
            route::{
                kern::utils::{
                    finish_response, kernel_notify_header, multicast_notify, RTMGRP_IPV4_IFADDR,
                    RTMGRP_IPV6_IFADDR,
                },
                message::{
                    attr::addr::AddrAttr,
                    segment::{
                        addr::{AddrMessageFlags, AddrSegment, AddrSegmentBody, RtScope},
                        RouteNlSegment,
                    },
                },
            },
        },
        AddressFamily,
    },
    process::namespace::net_namespace::NetNamespace,
};
use alloc::ffi::CString;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::num::NonZeroU32;
use smoltcp::wire::{IpAddress, IpCidr, Ipv4Address, Ipv6Address};
use system_error::SystemError;

pub(super) fn do_get_addr(
    request_segment: &AddrSegment,
    netns: Arc<NetNamespace>,
) -> Result<Vec<RouteNlSegment>, SystemError> {
    let dump_all = {
        let flags = GetRequestFlags::from_bits_truncate(request_segment.header().flags);
        flags.contains(GetRequestFlags::DUMP)
    };

    if !dump_all {
        log::error!("GetAddr request without DUMP flag is not supported yet");
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }

    let requested_index = request_segment.body().index.map(NonZeroU32::get);
    let requested_family = AddressFamily::try_from(request_segment.body().family as u16)
        .ok()
        .filter(|family| *family != AddressFamily::Unspecified);

    let mut responce: Vec<RouteNlSegment> = netns
        .device_list()
        .iter()
        .filter(|(_, iface)| requested_index.is_none_or(|index| iface.nic_id() as u32 == index))
        .flat_map(|(_, iface)| iface_to_new_addr(request_segment.header(), iface))
        .filter(|segment| {
            requested_family.is_none_or(|family| {
                AddressFamily::try_from(segment.body().family as u16)
                    .ok()
                    .is_some_and(|segment_family| segment_family == family)
            })
        })
        .map(RouteNlSegment::NewAddr)
        .collect();

    // getifaddrs(3) 期望全局地址列表按族排序：IPv4 在前、IPv6 在后。
    responce.sort_by_key(|segment| match segment {
        RouteNlSegment::NewAddr(addr) => addr.body().family,
        _ => 0,
    });

    finish_response(request_segment.header(), dump_all, &mut responce);

    Ok(responce)
}

pub(super) fn do_new_addr(
    request_segment: &AddrSegment,
    netns: Arc<NetNamespace>,
) -> Result<Vec<RouteNlSegment>, SystemError> {
    let (iface, cidr, changed) = add_addr(request_segment, netns.clone())?;
    if changed {
        multicast_notify(
            netns,
            addr_notify_group(cidr.address()),
            RouteNlSegment::NewAddr(addr_to_segment(
                &kernel_notify_header(CSegmentType::NEWADDR),
                &iface,
                cidr,
                CSegmentType::NEWADDR,
            )?),
        );
    }
    Ok(Vec::new())
}

pub(super) fn do_del_addr(
    request_segment: &AddrSegment,
    netns: Arc<NetNamespace>,
) -> Result<Vec<RouteNlSegment>, SystemError> {
    let (iface, cidr) = del_addr(request_segment, netns.clone())?;
    multicast_notify(
        netns,
        addr_notify_group(cidr.address()),
        RouteNlSegment::DelAddr(addr_to_segment(
            &kernel_notify_header(CSegmentType::DELADDR),
            &iface,
            cidr,
            CSegmentType::DELADDR,
        )?),
    );
    Ok(Vec::new())
}

fn add_addr(
    request_segment: &AddrSegment,
    netns: Arc<NetNamespace>,
) -> Result<(Arc<dyn Iface>, IpCidr, bool), SystemError> {
    let iface = lookup_iface_by_index(request_segment, netns)?;
    let cidr = parse_cidr(request_segment)?;
    let flags = NewRequestFlags::from_bits_truncate(request_segment.header().flags);

    let mut exists = false;
    let mut pushed = false;

    iface.smol_iface().lock().update_ip_addrs(|ip_addrs| {
        exists = ip_addrs.contains(&cidr);
        if !exists {
            let insert_index = match cidr.address() {
                IpAddress::Ipv4(_) => ip_addrs
                    .iter()
                    .position(|configured| matches!(configured.address(), IpAddress::Ipv6(_)))
                    .unwrap_or(ip_addrs.len()),
                IpAddress::Ipv6(_) => ip_addrs.len(),
            };

            if ip_addrs.insert(insert_index, cidr).is_ok() {
                pushed = true;
            }
        }
    });

    if exists {
        if flags.contains(NewRequestFlags::REPLACE) {
            return Ok((iface, cidr, false));
        }
        return Err(SystemError::EEXIST);
    }

    if !pushed {
        return Err(SystemError::ENOSPC);
    }

    if let Err(err) = add_local_route(&iface, cidr) {
        rollback_added_addr(&iface, cidr);
        return Err(err);
    }

    sync_router_ip_addrs(&iface);

    Ok((iface, cidr, true))
}

fn del_addr(
    request_segment: &AddrSegment,
    netns: Arc<NetNamespace>,
) -> Result<(Arc<dyn Iface>, IpCidr), SystemError> {
    let iface = lookup_iface_by_index(request_segment, netns)?;
    let cidr = parse_cidr(request_segment)?;

    let mut removed = false;

    iface.smol_iface().lock().update_ip_addrs(|ip_addrs| {
        if let Some(index) = ip_addrs.iter().position(|configured| *configured == cidr) {
            ip_addrs.remove(index);
            removed = true;
        }
    });

    if !removed {
        return Err(SystemError::EADDRNOTAVAIL);
    }

    remove_local_route(&iface, cidr);
    sync_router_ip_addrs(&iface);

    Ok((iface, cidr))
}

fn lookup_iface_by_index(
    request_segment: &AddrSegment,
    netns: Arc<NetNamespace>,
) -> Result<Arc<dyn Iface>, SystemError> {
    let index = request_segment
        .body()
        .index
        .ok_or(SystemError::EINVAL)?
        .get() as usize;

    netns
        .device_list()
        .get(&index)
        .cloned()
        .ok_or(SystemError::ENODEV)
}

fn parse_cidr(request_segment: &AddrSegment) -> Result<IpCidr, SystemError> {
    let family = AddressFamily::try_from(request_segment.body().family as u16)
        .map_err(|_| SystemError::EAFNOSUPPORT)?;

    let addr = request_segment
        .attrs()
        .iter()
        .find_map(|attr| match attr {
            AddrAttr::Local(addr) | AddrAttr::Address(addr) => Some(addr.as_slice()),
            AddrAttr::Label(_) => None,
        })
        .ok_or(SystemError::EINVAL)?;

    let prefix_len = request_segment.body().prefix_len;

    match family {
        AddressFamily::INet => {
            if addr.len() != 4 || prefix_len > 32 {
                return Err(SystemError::EINVAL);
            }
            let ip = Ipv4Address::new(addr[0], addr[1], addr[2], addr[3]);
            Ok(IpCidr::new(IpAddress::Ipv4(ip), prefix_len))
        }
        AddressFamily::INet6 => {
            if addr.len() != 16 || prefix_len > 128 {
                return Err(SystemError::EINVAL);
            }
            let ip = Ipv6Address::new(
                u16::from_be_bytes([addr[0], addr[1]]),
                u16::from_be_bytes([addr[2], addr[3]]),
                u16::from_be_bytes([addr[4], addr[5]]),
                u16::from_be_bytes([addr[6], addr[7]]),
                u16::from_be_bytes([addr[8], addr[9]]),
                u16::from_be_bytes([addr[10], addr[11]]),
                u16::from_be_bytes([addr[12], addr[13]]),
                u16::from_be_bytes([addr[14], addr[15]]),
            );
            Ok(IpCidr::new(IpAddress::Ipv6(ip), prefix_len))
        }
        _ => Err(SystemError::EAFNOSUPPORT),
    }
}

fn iface_to_new_addr(request_header: &CMsgSegHdr, iface: &Arc<dyn Iface>) -> Vec<AddrSegment> {
    let mut segments = Vec::new();
    let ip_addrs: Vec<IpCidr> = {
        let smol_iface = iface.smol_iface().lock();
        smol_iface.ip_addrs().to_vec()
    };

    for cidr in &ip_addrs {
        if let Ok(segment) = addr_to_segment(
            request_header,
            iface,
            *cidr,
            CSegmentType::NEWADDR,
        ) {
            segments.push(segment);
        }
    }

    segments
}

fn addr_to_segment(
    request_header: &CMsgSegHdr,
    iface: &Arc<dyn Iface>,
    cidr: IpCidr,
    msg_type: CSegmentType,
) -> Result<AddrSegment, SystemError> {
    let (family, octets): (i32, Vec<u8>) = match cidr.address() {
        IpAddress::Ipv4(addr) => (AddressFamily::INet as i32, addr.octets().to_vec()),
        IpAddress::Ipv6(addr) => (AddressFamily::INet6 as i32, addr.octets().to_vec()),
    };

    let header = CMsgSegHdr {
        len: 0,
        type_: msg_type as _,
        flags: SegHdrCommonFlags::empty().bits(),
        seq: request_header.seq,
        pid: request_header.pid,
    };

    let addr_message = AddrSegmentBody {
        family,
        prefix_len: cidr.prefix_len(),
        flags: AddrMessageFlags::PERMANENT,
        scope: if iface.name() == "lo" {
            RtScope::HOST
        } else {
            RtScope::UNIVERSE
        },
        index: NonZeroU32::new(iface.nic_id() as u32),
    };

    let attrs = vec![
        AddrAttr::Address(octets.clone()),
        AddrAttr::Label(CString::new(iface.iface_name()).map_err(|_| SystemError::EINVAL)?),
        AddrAttr::Local(octets),
    ];

    Ok(AddrSegment::new(header, addr_message, attrs))
}

fn sync_router_ip_addrs(iface: &Arc<dyn Iface>) {
    let smol_ip_addrs: Vec<IpCidr> = {
        let smol_iface = iface.smol_iface().lock();
        smol_iface.ip_addrs().to_vec()
    };

    let mut router_ip_addrs = iface.router_common().ip_addrs.write();
    router_ip_addrs.clear();
    router_ip_addrs.extend_from_slice(&smol_ip_addrs);
}

fn add_local_route(iface: &Arc<dyn Iface>, cidr: IpCidr) -> Result<(), SystemError> {
    let mut pushed = false;
    let via_router = cidr.address();

    iface.smol_iface().lock().routes_mut().update(|routes| {
        let exists = routes
            .iter()
            .any(|route| is_same_local_route(route, cidr, via_router));
        if exists {
            pushed = true;
            return;
        }

        pushed = routes
            .push(smoltcp::iface::Route {
                cidr,
                via_router,
                preferred_until: None,
                expires_at: None,
            })
            .is_ok();
    });

    if !pushed {
        log::warn!(
            "netlink add_addr: route table full while adding local route {} via {}",
            cidr,
            via_router
        );
        return Err(SystemError::ENOSPC);
    }

    Ok(())
}

fn remove_local_route(iface: &Arc<dyn Iface>, cidr: IpCidr) {
    let via_router = cidr.address();
    iface.smol_iface().lock().routes_mut().update(|routes| {
        if let Some(index) = routes
            .iter()
            .position(|route| is_same_local_route(route, cidr, via_router))
        {
            routes.remove(index);
        }
    });
}

fn rollback_added_addr(iface: &Arc<dyn Iface>, cidr: IpCidr) {
    iface.smol_iface().lock().update_ip_addrs(|ip_addrs| {
        if let Some(index) = ip_addrs.iter().position(|configured| *configured == cidr) {
            ip_addrs.remove(index);
        }
    });
    sync_router_ip_addrs(iface);
}

#[inline]
fn is_same_local_route(route: &smoltcp::iface::Route, cidr: IpCidr, via_router: IpAddress) -> bool {
    route.cidr == cidr && route.via_router == via_router
}

fn addr_notify_group(ip: IpAddress) -> u32 {
    match ip {
        IpAddress::Ipv4(_) => RTMGRP_IPV4_IFADDR,
        IpAddress::Ipv6(_) => RTMGRP_IPV6_IFADDR,
    }
}
