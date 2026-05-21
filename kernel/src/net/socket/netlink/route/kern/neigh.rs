use crate::{
    driver::net::StaticNeighborEntry,
    net::socket::{
        netlink::{
            message::segment::{
                header::{CMsgSegHdr, GetRequestFlags, SegHdrCommonFlags},
                CSegmentType,
            },
            route::{
                kern::utils::{
                    finish_response, kernel_notify_header, multicast_notify, RTMGRP_NEIGH,
                },
                message::segment::neigh::{NeighSegmentBody, NeighState},
            },
        },
        netlink::route::message::{
            attr::neigh::NeighAttr,
            segment::{neigh::NeighSegment, RouteNlSegment},
        },
        AddressFamily,
    },
    process::namespace::net_namespace::NetNamespace,
};
use alloc::sync::Arc;
use alloc::vec::Vec;
use smoltcp::wire::{EthernetAddress, HardwareAddress, IpAddress, Ipv4Address, Ipv6Address};
use system_error::SystemError;

pub(super) fn do_get_neigh(
    request_segment: &NeighSegment,
    netns: Arc<NetNamespace>,
) -> Result<Vec<RouteNlSegment>, SystemError> {
    let dump_all = GetRequestFlags::from_bits_truncate(request_segment.header().flags)
        .contains(GetRequestFlags::DUMP);
    if !dump_all {
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }

    let requested_family = request_segment.body().family;
    let requested_ifindex = if request_segment.body().ifindex > 0 {
        Some(request_segment.body().ifindex as usize)
    } else {
        None
    };

    let mut response = Vec::new();
    for (_, iface) in netns.device_list().iter() {
        if requested_ifindex.is_some_and(|ifindex| iface.nic_id() != ifindex) {
            continue;
        }

        for entry in iface.common().static_neighbors().iter() {
            if !family_matches(requested_family, family_of_ip(entry.ip_addr)) {
                continue;
            }
            response.push(RouteNlSegment::NewNeigh(neigh_to_segment(
                request_segment.header(),
                iface.nic_id() as i32,
                entry,
                CSegmentType::NEWNEIGH,
            )));
        }
    }

    finish_response(request_segment.header(), true, &mut response);
    Ok(response)
}

pub(super) fn do_new_neigh(
    request_segment: &NeighSegment,
    netns: Arc<NetNamespace>,
) -> Result<Vec<RouteNlSegment>, SystemError> {
    if request_segment.body().ifindex <= 0 {
        return Err(SystemError::EINVAL);
    }
    let ifindex =
        usize::try_from(request_segment.body().ifindex).map_err(|_| SystemError::EINVAL)?;
    let iface = netns
        .device_list()
        .get(&ifindex)
        .cloned()
        .ok_or(SystemError::ENODEV)?;
    let family = request_segment.body().family;
    let ip = request_segment
        .attrs()
        .iter()
        .find_map(|attr| match attr {
            NeighAttr::Destination(bytes) => Some(bytes.as_slice()),
            NeighAttr::LinkLocalAddress(_) => None,
        })
        .ok_or(SystemError::EINVAL)
        .and_then(|bytes| parse_ip(bytes, family))?;
    let lladdr_bytes = request_segment.attrs().iter().find_map(|attr| match attr {
        NeighAttr::LinkLocalAddress(bytes) => Some(bytes.as_slice()),
        NeighAttr::Destination(_) => None,
    });
    let hw_addr = match lladdr_bytes {
        Some(bytes) => parse_mac(bytes)?,
        None => return Err(SystemError::EINVAL),
    };

    let entry = StaticNeighborEntry {
        ip_addr: ip,
        hw_addr,
        state: request_segment.body().state.bits(),
        flags: request_segment.body().flags,
    };
    iface.common().set_static_neighbor(entry.clone());
    multicast_notify(
        netns,
        RTMGRP_NEIGH,
        RouteNlSegment::NewNeigh(neigh_to_segment(
            &kernel_notify_header(CSegmentType::NEWNEIGH),
            iface.nic_id() as i32,
            &entry,
            CSegmentType::NEWNEIGH,
        )),
    );
    Ok(Vec::new())
}

pub(super) fn do_del_neigh(
    request_segment: &NeighSegment,
    netns: Arc<NetNamespace>,
) -> Result<Vec<RouteNlSegment>, SystemError> {
    if request_segment.body().ifindex <= 0 {
        return Err(SystemError::EINVAL);
    }
    let ifindex =
        usize::try_from(request_segment.body().ifindex).map_err(|_| SystemError::EINVAL)?;
    let iface = netns
        .device_list()
        .get(&ifindex)
        .cloned()
        .ok_or(SystemError::ENODEV)?;
    let family = request_segment.body().family;
    let ip = request_segment
        .attrs()
        .iter()
        .find_map(|attr| match attr {
            NeighAttr::Destination(bytes) => Some(bytes.as_slice()),
            NeighAttr::LinkLocalAddress(_) => None,
        })
        .ok_or(SystemError::EINVAL)
        .and_then(|bytes| parse_ip(bytes, family))?;

    if !iface.common().remove_static_neighbor(ip) {
        return Err(SystemError::ENOENT);
    }

    multicast_notify(
        netns,
        RTMGRP_NEIGH,
        RouteNlSegment::DelNeigh(neigh_to_segment(
            &kernel_notify_header(CSegmentType::DELNEIGH),
            iface.nic_id() as i32,
            &StaticNeighborEntry {
                ip_addr: ip,
                hw_addr: HardwareAddress::Ethernet(EthernetAddress::default()),
                state: request_segment.body().state.bits(),
                flags: request_segment.body().flags,
            },
            CSegmentType::DELNEIGH,
        )),
    );

    Ok(Vec::new())
}

fn neigh_to_segment(
    request_header: &CMsgSegHdr,
    ifindex: i32,
    entry: &StaticNeighborEntry,
    msg_type: CSegmentType,
) -> NeighSegment {
    let header = crate::net::socket::netlink::message::segment::header::CMsgSegHdr {
        len: 0,
        type_: msg_type as u16,
        flags: SegHdrCommonFlags::empty().bits(),
        seq: request_header.seq,
        pid: request_header.pid,
    };
    let body = NeighSegmentBody {
        family: family_of_ip(entry.ip_addr),
        ifindex,
        state: NeighState::from_bits_truncate(entry.state),
        flags: entry.flags,
        kind: crate::net::socket::netlink::route::message::segment::route::RouteType::Unicast,
    };
    let mut attrs = vec![NeighAttr::Destination(ip_to_bytes(entry.ip_addr))];
    if let HardwareAddress::Ethernet(addr) = entry.hw_addr {
        attrs.push(NeighAttr::LinkLocalAddress(addr.as_bytes().to_vec()));
    }
    NeighSegment::new(header, body, attrs)
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

fn ip_to_bytes(ip: IpAddress) -> Vec<u8> {
    match ip {
        IpAddress::Ipv4(addr) => addr.octets().to_vec(),
        IpAddress::Ipv6(addr) => addr.octets().to_vec(),
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

fn parse_mac(bytes: &[u8]) -> Result<HardwareAddress, SystemError> {
    if bytes.len() != 6 {
        return Err(SystemError::EINVAL);
    }

    let mut mac = [0u8; 6];
    mac.copy_from_slice(bytes);
    Ok(HardwareAddress::Ethernet(EthernetAddress(mac)))
}
