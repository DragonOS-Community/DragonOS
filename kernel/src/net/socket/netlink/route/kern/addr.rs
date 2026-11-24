use crate::{
    driver::net::{AddressState, DadState, Iface},
    net::socket::{
        netlink::{
            message::segment::{
                header::{CMsgSegHdr, NewRequestFlags, SegHdrCommonFlags},
                CSegmentType,
            },
            route::{
                kern::utils::{
                    kernel_notify_header, multicast_notify, RTMGRP_IPV4_IFADDR, RTMGRP_IPV6_IFADDR,
                },
                message::{
                    attr::{addr::AddrAttr, IFNAME_SIZE},
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
        address::{
            address_info, AddressInfo, AddressMutation, AddressMutationOutcome, AddressOptions,
        },
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

pub(super) fn do_new_addr(
    rtnl: &RtnlGuard,
    request_segment: &AddrSegment,
    netns: Arc<NetNamespace>,
) -> Result<Vec<RouteNlSegment>, SystemError> {
    // Linux applies the IPv4 attribute policy before address, device, and
    // special-case validation. Preserve that errno priority here.
    let broadcast = parse_request_broadcast(request_segment)?;
    let label = parse_request_label(request_segment)?;
    let ipv6_nodad = (request_segment.body().family == AddressFamily::INet6 as i32)
        .then(|| request_ipv6_nodad(request_segment))
        .transpose()?;
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
    let commit = crate::net::address::mutate_address_with_options(
        rtnl,
        &iface,
        mutation,
        AddressOptions {
            label,
            broadcast,
            ipv6_nodad,
        },
    )?;
    match commit.outcome {
        AddressMutationOutcome::Added(cidr) => notify_address_change(netns.clone(), &iface, cidr),
        AddressMutationOutcome::Replaced(cidr)
            if crate::net::address::address_state(&iface, cidr).is_usable() =>
        {
            notify_address_change(netns.clone(), &iface, cidr);
        }
        _ => {}
    }
    notify_route_changes(&netns, commit.route_changes);
    Ok(Vec::new())
}

fn request_ipv6_nodad(request: &AddrSegment) -> Result<bool, SystemError> {
    let mut flags = request.body().flags;
    for attr in request.attrs() {
        if let AddrAttr::Flags(bytes) = attr {
            let raw = bytes.first_chunk::<4>().ok_or(SystemError::ERANGE)?;
            flags = AddrMessageFlags::from_bits_truncate(u32::from_ne_bytes(*raw));
        }
    }
    Ok(flags.contains(AddrMessageFlags::NODAD))
}

pub(super) fn do_del_addr(
    rtnl: &RtnlGuard,
    request_segment: &AddrSegment,
    netns: Arc<NetNamespace>,
) -> Result<Vec<RouteNlSegment>, SystemError> {
    // Linux validates the IPv4 attribute policy even though IFA_BROADCAST
    // does not participate in the delete selector.
    let _ = parse_request_broadcast(request_segment)?;
    let label = parse_request_label(request_segment)?;
    let selector = parse_delete_selector(request_segment, label)?;
    let iface = lookup_iface_by_index(request_segment, &netns)?;
    let cidr = resolve_delete_cidr(&iface, selector)?;
    let deleted = address_info(&iface, cidr)?;
    let commit = crate::net::address::mutate_address(rtnl, &iface, AddressMutation::Delete(cidr))?;
    notify_removed_address(netns.clone(), &iface, &deleted);
    notify_route_changes(&netns, commit.route_changes);
    Ok(Vec::new())
}

fn notify_route_changes(netns: &Arc<NetNamespace>, changes: crate::net::route::RouteNotifications) {
    for removed in changes.removed {
        super::route::notify_route(netns, CSegmentType::DELROUTE, removed);
    }
    for added in changes.added {
        super::route::notify_route(netns, CSegmentType::NEWROUTE, added);
    }
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

fn parse_delete_selector(
    request_segment: &AddrSegment,
    label: Option<CString>,
) -> Result<DeleteSelector, SystemError> {
    let family = AddressFamily::try_from(request_segment.body().family as u16)
        .map_err(|_| SystemError::EAFNOSUPPORT)?;
    let prefix_len = request_segment.body().prefix_len;
    let local = find_address_attr(request_segment, true);
    let address = find_address_attr(request_segment, false);
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
    if let DeleteMatch::Exact(cidr) = selector.match_ {
        return crate::net::address::has_address_object(iface, cidr)
            .then_some(cidr)
            .ok_or(SystemError::EADDRNOTAVAIL);
    }
    let smol_iface = iface.smol_iface().lock();
    smol_iface
        .ip_addrs()
        .iter()
        .find(|configured| {
            if let Some(requested) = selector.label.as_ref() {
                let Ok(actual) = crate::net::address::address_label(iface, **configured) else {
                    return false;
                };
                if actual.as_bytes() != requested.as_bytes() {
                    return false;
                }
            }
            match selector.match_ {
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
            }
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

fn parse_ipv4_label_attr(request_segment: &AddrSegment) -> Result<Option<CString>, SystemError> {
    let mut parsed = None;
    for attr in request_segment.attrs() {
        if let AddrAttr::Label(payload) = attr {
            parsed = Some(parse_ipv4_label(payload)?);
        }
    }
    Ok(parsed)
}

fn parse_request_label(request_segment: &AddrSegment) -> Result<Option<CString>, SystemError> {
    // IFA_LABEL is an IPv4 alias attribute. Linux's IPv6 policy leaves it
    // untyped and the IPv6 handlers ignore it, regardless of payload length.
    match AddressFamily::try_from(request_segment.body().family as u16) {
        Ok(AddressFamily::INet) => parse_ipv4_label_attr(request_segment),
        _ => Ok(None),
    }
}

fn parse_request_broadcast(
    request_segment: &AddrSegment,
) -> Result<Option<Ipv4Address>, SystemError> {
    if !matches!(
        AddressFamily::try_from(request_segment.body().family as u16),
        Ok(AddressFamily::INet)
    ) {
        return Ok(None);
    }
    let mut broadcast = None;
    for attr in request_segment.attrs() {
        if let AddrAttr::Broadcast(bytes) = attr {
            if bytes.len() < 4 {
                return Err(SystemError::ERANGE);
            }
            broadcast = Some(Ipv4Address::new(bytes[0], bytes[1], bytes[2], bytes[3]));
        }
    }
    Ok(broadcast)
}

fn parse_ipv4_label(payload: &[u8]) -> Result<CString, SystemError> {
    if payload.is_empty() {
        return Err(SystemError::ERANGE);
    }
    let effective_len = payload.len() - usize::from(payload.last() == Some(&0));
    if effective_len >= IFNAME_SIZE {
        return Err(SystemError::ERANGE);
    }
    let nul_pos = payload
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(payload.len());
    CString::new(&payload[..nul_pos]).map_err(|_| SystemError::EINVAL)
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

pub(super) fn addr_to_segment(
    request_header: &CMsgSegHdr,
    iface: &Arc<dyn Iface>,
    address: &AddressInfo,
    msg_type: CSegmentType,
) -> AddrSegment {
    let cidr = address.cidr;
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

    let slaac = matches!(address.state, AddressState::Slaac(_));
    let mut flags = if slaac {
        AddrMessageFlags::MANAGETEMPADDR
    } else {
        AddrMessageFlags::PERMANENT
    };
    if address.ipv6_nodad {
        flags.insert(AddrMessageFlags::NODAD);
    }
    match address.state {
        AddressState::AutoLinkLocal(DadState::Tentative)
        | AddressState::Slaac(DadState::Tentative)
        | AddressState::ManualDad(DadState::Tentative) => {
            flags.insert(AddrMessageFlags::TENTATIVE);
        }
        AddressState::AutoLinkLocal(DadState::DadFailed)
        | AddressState::ManualDad(DadState::DadFailed) => {
            flags.insert(AddrMessageFlags::TENTATIVE | AddrMessageFlags::DADFAILED);
        }
        _ => {}
    }
    let now_us = crate::time::Instant::now().total_micros();
    if address
        .lifetimes
        .is_some_and(|lifetimes| lifetimes.deprecated(now_us))
    {
        flags.insert(AddrMessageFlags::DEPRECATED);
    }
    let addr_message = AddrSegmentBody {
        family,
        prefix_len: cidr.prefix_len(),
        flags,
        scope: match cidr.address() {
            IpAddress::Ipv6(address) if address.is_loopback() => RtScope::HOST,
            address if crate::net::route::is_ipv6_link_local(address) => RtScope::LINK,
            IpAddress::Ipv4(_)
                if iface.type_() == crate::driver::net::types::InterfaceType::LOOPBACK =>
            {
                RtScope::HOST
            }
            _ => RtScope::UNIVERSE,
        },
        index: NonZeroU32::new(iface.nic_id() as u32),
    };

    // Match Linux inet_fill_ifaddr(): IPv4 ADDRESS, LOCAL, BROADCAST, LABEL.
    let mut attrs = vec![AddrAttr::Address(octets.clone()), AddrAttr::Local(octets)];
    if matches!(cidr.address(), IpAddress::Ipv4(_)) {
        if let Some(broadcast) = address
            .broadcast
            .filter(|address| !address.is_unspecified())
        {
            attrs.push(AddrAttr::Broadcast(broadcast.octets().to_vec()));
        }
        // Linux omits IFA_LABEL for an explicit empty IPv4 label.
        if !address.label.as_bytes().is_empty() {
            attrs.push(AddrAttr::Label(address.label.to_bytes_with_nul().to_vec()));
        }
    } else {
        if let Some(lifetimes) = address.lifetimes {
            let mut cache = [0; 16];
            for (slot, value) in cache.chunks_exact_mut(4).zip(lifetimes.cache_info(now_us)) {
                slot.copy_from_slice(&value.to_ne_bytes());
            }
            attrs.push(AddrAttr::CacheInfo(cache));
        }
        if slaac || address.ipv6_nodad {
            attrs.push(AddrAttr::Flags(flags.bits().to_ne_bytes().to_vec()));
        }
        if slaac {
            const IFAPROT_KERNEL_RA: u8 = 2;
            attrs.push(AddrAttr::Proto([IFAPROT_KERNEL_RA]));
        }
    }

    AddrSegment::new(header, addr_message, attrs)
}

pub(super) fn notify_address_change(
    netns: Arc<NetNamespace>,
    iface: &Arc<dyn Iface>,
    cidr: IpCidr,
) {
    match address_info(iface, cidr) {
        Ok(address) => notify_one(netns, iface, &address, CSegmentType::NEWADDR),
        Err(error) => log::warn!("failed to snapshot address notification for {cidr}: {error:?}"),
    }
}

pub(super) fn notify_removed_address(
    netns: Arc<NetNamespace>,
    iface: &Arc<dyn Iface>,
    address: &AddressInfo,
) {
    notify_one(netns, iface, address, CSegmentType::DELADDR);
}

fn notify_one(
    netns: Arc<NetNamespace>,
    iface: &Arc<dyn Iface>,
    address: &AddressInfo,
    msg_type: CSegmentType,
) {
    let header = kernel_notify_header(msg_type);
    let segment = addr_to_segment(&header, iface, address, msg_type);
    let segment = if msg_type == CSegmentType::DELADDR {
        RouteNlSegment::DelAddr(segment)
    } else {
        RouteNlSegment::NewAddr(segment)
    };
    multicast_notify(netns, addr_notify_group(address.cidr.address()), segment);
}

fn addr_notify_group(ip: IpAddress) -> u32 {
    match ip {
        IpAddress::Ipv4(_) => RTMGRP_IPV4_IFADDR,
        IpAddress::Ipv6(_) => RTMGRP_IPV6_IFADDR,
    }
}
