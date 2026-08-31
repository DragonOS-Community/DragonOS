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
    net::{
        address::{AddressMutation, AddressMutationOutcome},
        rtnl::RtnlGuard,
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
    rtnl: &RtnlGuard,
    request_segment: &AddrSegment,
    netns: Arc<NetNamespace>,
) -> Result<Vec<RouteNlSegment>, SystemError> {
    let cidr = parse_new_cidr(request_segment)?;
    let iface = lookup_iface_by_index(request_segment, &netns)?;

    match cidr.address() {
        // Linux accepts an IPv4 local address of zero but does not insert an
        // in_ifaddr object or emit RTM_NEWADDR for it.
        IpAddress::Ipv4(address) if address.is_unspecified() => return Ok(Vec::new()),
        // inet6_addr_add() rejects IPV6_ADDR_ANY as a configured object.
        IpAddress::Ipv6(address) if address.is_unspecified() => {
            return Err(SystemError::EADDRNOTAVAIL)
        }
        _ => {}
    }

    let flags = NewRequestFlags::from_bits_truncate(request_segment.header().flags);
    let mutation =
        if flags.contains(NewRequestFlags::REPLACE) && !flags.contains(NewRequestFlags::EXCL) {
            AddressMutation::Replace(cidr)
        } else {
            AddressMutation::Add(cidr)
        };
    let outcome = crate::net::address::mutate_address(rtnl, &iface, mutation)?;
    notify_address_outcome(netns, &iface, outcome);
    Ok(Vec::new())
}

pub(super) fn do_del_addr(
    rtnl: &RtnlGuard,
    request_segment: &AddrSegment,
    netns: Arc<NetNamespace>,
) -> Result<Vec<RouteNlSegment>, SystemError> {
    let selector = parse_delete_selector(request_segment)?;
    let iface = lookup_iface_by_index(request_segment, &netns)?;
    let cidr = resolve_delete_cidr(&iface, selector)?;
    let outcome = crate::net::address::mutate_address(rtnl, &iface, AddressMutation::Delete(cidr))?;
    notify_address_outcome(netns, &iface, outcome);
    Ok(Vec::new())
}

fn lookup_iface_by_index(
    request_segment: &AddrSegment,
    netns: &Arc<NetNamespace>,
) -> Result<Arc<dyn Iface>, SystemError> {
    let index = request_segment
        .body()
        .index
        .ok_or(SystemError::ENODEV)?
        .get() as usize;

    netns
        .device_list()
        .get(&index)
        .cloned()
        .ok_or(SystemError::ENODEV)
}

fn parse_new_cidr(request_segment: &AddrSegment) -> Result<IpCidr, SystemError> {
    let family = AddressFamily::try_from(request_segment.body().family as u16)
        .map_err(|_| SystemError::EAFNOSUPPORT)?;
    let prefix_len = request_segment.body().prefix_len;
    let local = find_address_attr(request_segment, true);
    let address = find_address_attr(request_segment, false);

    match family {
        AddressFamily::INet => {
            let local = local.ok_or(SystemError::EINVAL)?;
            if let Some(address) = address {
                if address != local {
                    return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
                }
            }
            parse_ip_cidr(family, local, prefix_len)
        }
        AddressFamily::INet6 => {
            let bytes = address.or(local).ok_or(SystemError::EINVAL)?;
            if let (Some(local), Some(address)) = (local, address) {
                if local != address {
                    return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
                }
            }
            parse_ip_cidr(family, bytes, prefix_len)
        }
        _ => Err(SystemError::EAFNOSUPPORT),
    }
}

enum DeleteMatch {
    Ipv4 {
        local: Option<Ipv4Address>,
        prefix: Option<IpCidr>,
    },
    Exact(IpCidr),
}

struct DeleteSelector {
    match_: DeleteMatch,
    label: Option<CString>,
}

fn parse_delete_selector(request_segment: &AddrSegment) -> Result<DeleteSelector, SystemError> {
    let family = AddressFamily::try_from(request_segment.body().family as u16)
        .map_err(|_| SystemError::EAFNOSUPPORT)?;
    let prefix_len = request_segment.body().prefix_len;
    let local = find_address_attr(request_segment, true);
    let address = find_address_attr(request_segment, false);
    let label = request_segment.attrs().iter().find_map(|attr| match attr {
        AddrAttr::Label(label) => Some(label.clone()),
        _ => None,
    });

    let match_ = match family {
        AddressFamily::INet => {
            let local = local
                .map(|bytes| parse_ip_cidr(family, bytes, 32))
                .transpose()?
                .map(|cidr| match cidr.address() {
                    IpAddress::Ipv4(address) => address,
                    _ => unreachable!(),
                });
            let prefix = if let Some(address) = address {
                if prefix_len > 32 {
                    return Err(SystemError::EINVAL);
                }
                Some(parse_ip_cidr(family, address, prefix_len)?)
            } else {
                None
            };
            if local.is_none() && prefix.is_none() {
                return Err(SystemError::EINVAL);
            }
            DeleteMatch::Ipv4 { local, prefix }
        }
        AddressFamily::INet6 => {
            let bytes = address.or(local).ok_or(SystemError::EINVAL)?;
            if let (Some(local), Some(address)) = (local, address) {
                if local != address {
                    return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
                }
            }
            DeleteMatch::Exact(parse_ip_cidr(family, bytes, prefix_len)?)
        }
        _ => return Err(SystemError::EAFNOSUPPORT),
    };

    Ok(DeleteSelector { match_, label })
}

fn resolve_delete_cidr(
    iface: &Arc<dyn Iface>,
    selector: DeleteSelector,
) -> Result<IpCidr, SystemError> {
    if selector
        .label
        .as_ref()
        .is_some_and(|label| label.to_bytes() != iface.iface_name().as_bytes())
    {
        return Err(SystemError::EADDRNOTAVAIL);
    }

    let smol_iface = iface.smol_iface().lock();
    smol_iface
        .ip_addrs()
        .iter()
        .find(|configured| match selector.match_ {
            DeleteMatch::Ipv4 { local, prefix } => {
                let IpAddress::Ipv4(configured_address) = configured.address() else {
                    return false;
                };
                local.is_none_or(|local| local == configured_address)
                    && prefix.is_none_or(|prefix| {
                        prefix.prefix_len() == configured.prefix_len()
                            && prefix.contains_addr(&configured.address())
                    })
            }
            DeleteMatch::Exact(cidr) => **configured == cidr,
        })
        .copied()
        .ok_or(SystemError::EADDRNOTAVAIL)
}

fn find_address_attr(request_segment: &AddrSegment, local: bool) -> Option<&[u8]> {
    request_segment.attrs().iter().find_map(|attr| match attr {
        AddrAttr::Local(bytes) if local => Some(bytes.as_slice()),
        AddrAttr::Address(bytes) if !local => Some(bytes.as_slice()),
        _ => None,
    })
}

fn parse_ip_cidr(
    family: AddressFamily,
    addr: &[u8],
    prefix_len: u8,
) -> Result<IpCidr, SystemError> {
    match family {
        AddressFamily::INet => {
            if addr.len() != 4 || prefix_len > 32 {
                return Err(SystemError::EINVAL);
            }
            Ok(IpCidr::new(
                IpAddress::Ipv4(Ipv4Address::new(addr[0], addr[1], addr[2], addr[3])),
                prefix_len,
            ))
        }
        AddressFamily::INet6 => {
            if addr.len() != 16 || prefix_len > 128 {
                return Err(SystemError::EINVAL);
            }
            Ok(IpCidr::new(
                IpAddress::Ipv6(Ipv6Address::new(
                    u16::from_be_bytes([addr[0], addr[1]]),
                    u16::from_be_bytes([addr[2], addr[3]]),
                    u16::from_be_bytes([addr[4], addr[5]]),
                    u16::from_be_bytes([addr[6], addr[7]]),
                    u16::from_be_bytes([addr[8], addr[9]]),
                    u16::from_be_bytes([addr[10], addr[11]]),
                    u16::from_be_bytes([addr[12], addr[13]]),
                    u16::from_be_bytes([addr[14], addr[15]]),
                )),
                prefix_len,
            ))
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
        if let Ok(segment) = addr_to_segment(request_header, iface, *cidr, CSegmentType::NEWADDR) {
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

pub(in crate::net::socket::netlink::route) fn notify_address_outcome(
    netns: Arc<NetNamespace>,
    iface: &Arc<dyn Iface>,
    outcome: AddressMutationOutcome,
) {
    match outcome {
        AddressMutationOutcome::Added(cidr) | AddressMutationOutcome::Replaced(cidr) => {
            notify_one(netns, iface, cidr, CSegmentType::NEWADDR)
        }
        AddressMutationOutcome::Deleted(cidr) => {
            notify_one(netns, iface, cidr, CSegmentType::DELADDR)
        }
        AddressMutationOutcome::Exchanged { old, new } => {
            notify_one(netns.clone(), iface, old, CSegmentType::DELADDR);
            notify_one(netns, iface, new, CSegmentType::NEWADDR);
        }
    }
}

fn notify_one(
    netns: Arc<NetNamespace>,
    iface: &Arc<dyn Iface>,
    cidr: IpCidr,
    msg_type: CSegmentType,
) {
    let header = kernel_notify_header(msg_type);
    let segment = match addr_to_segment(&header, iface, cidr, msg_type) {
        Ok(segment) => segment,
        Err(err) => {
            // Notification encoding is best effort after a successful commit;
            // it must not turn the request ACK into an error for committed state.
            log::warn!("failed to encode address notification for {cidr}: {err:?}");
            return;
        }
    };
    let segment = if msg_type == CSegmentType::DELADDR {
        RouteNlSegment::DelAddr(segment)
    } else {
        RouteNlSegment::NewAddr(segment)
    };
    multicast_notify(netns, addr_notify_group(cidr.address()), segment);
}

fn addr_notify_group(ip: IpAddress) -> u32 {
    match ip {
        IpAddress::Ipv4(_) => RTMGRP_IPV4_IFADDR,
        IpAddress::Ipv6(_) => RTMGRP_IPV6_IFADDR,
    }
}
