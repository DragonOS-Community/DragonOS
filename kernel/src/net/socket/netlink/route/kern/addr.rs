use crate::{
    driver::net::Iface,
    net::socket::{
        netlink::{
            message::segment::{
                header::{CMsgSegHdr, GetRequestFlags, NewRequestFlags, SegHdrCommonFlags},
                CSegmentType,
            },
            route::{
                kern::utils::finish_response,
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

    let mut responce: Vec<RouteNlSegment> = netns
        .device_list()
        .iter()
        .flat_map(|(_, iface)| iface_to_new_addr(request_segment.header(), iface))
        .map(RouteNlSegment::NewAddr)
        .collect();

    finish_response(request_segment.header(), dump_all, &mut responce);

    Ok(responce)
}

pub(super) fn do_new_addr(
    request_segment: &AddrSegment,
    netns: Arc<NetNamespace>,
) -> Result<Vec<RouteNlSegment>, SystemError> {
    add_addr(request_segment, netns)?;
    Ok(Vec::new())
}

pub(super) fn do_del_addr(
    request_segment: &AddrSegment,
    netns: Arc<NetNamespace>,
) -> Result<Vec<RouteNlSegment>, SystemError> {
    del_addr(request_segment, netns)?;
    Ok(Vec::new())
}

fn add_addr(request_segment: &AddrSegment, netns: Arc<NetNamespace>) -> Result<(), SystemError> {
    let iface = lookup_iface_by_index(request_segment, netns)?;
    let cidr = parse_cidr(request_segment)?;
    let flags = NewRequestFlags::from_bits_truncate(request_segment.header().flags);

    let mut exists = false;
    let mut pushed = true;

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

            if ip_addrs.insert(insert_index, cidr).is_err() {
                pushed = false;
            }
        }
    });

    if exists {
        if flags.contains(NewRequestFlags::REPLACE) {
            return Ok(());
        }
        return Err(SystemError::EEXIST);
    }

    if flags.contains(NewRequestFlags::REPLACE) {
        return Err(SystemError::ENOENT);
    }

    if !pushed {
        return Err(SystemError::ENOSPC);
    }

    sync_router_ip_addrs(&iface);

    Ok(())
}

fn del_addr(request_segment: &AddrSegment, netns: Arc<NetNamespace>) -> Result<(), SystemError> {
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

    sync_router_ip_addrs(&iface);

    Ok(())
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
    let ifname = CString::new(iface.iface_name()).unwrap();
    let mut segments = Vec::new();
    let ip_addrs: Vec<IpCidr> = {
        let smol_iface = iface.smol_iface().lock();
        smol_iface.ip_addrs().to_vec()
    };

    for cidr in &ip_addrs {
        let (family, octets): (i32, Vec<u8>) = match cidr.address() {
            IpAddress::Ipv4(addr) => (AddressFamily::INet as i32, addr.octets().to_vec()),
            IpAddress::Ipv6(addr) => (AddressFamily::INet6 as i32, addr.octets().to_vec()),
        };

        let header = CMsgSegHdr {
            len: 0,
            type_: CSegmentType::NEWADDR as _,
            flags: SegHdrCommonFlags::empty().bits(),
            seq: request_header.seq,
            pid: request_header.pid,
        };

        let addr_message = AddrSegmentBody {
            family,
            prefix_len: cidr.prefix_len(),
            flags: AddrMessageFlags::PERMANENT,
            scope: RtScope::HOST,
            index: NonZeroU32::new(iface.nic_id() as u32),
        };

        let attrs = vec![
            AddrAttr::Address(octets.clone()),
            AddrAttr::Label(ifname.clone()),
            AddrAttr::Local(octets),
        ];

        segments.push(AddrSegment::new(header, addr_message, attrs));
    }

    segments
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
