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

    finish_response(request_segment.header(), dump_all, &mut responce)?;

    Ok(responce)
}

pub(super) fn do_new_addr(
    rtnl: &RtnlGuard,
    request_segment: &AddrSegment,
    netns: Arc<NetNamespace>,
) -> Result<Vec<RouteNlSegment>, SystemError> {
    // Linux applies the IPv4 attribute policy before address, device, and
    // special-case validation. Preserve that errno priority here.
    let label = parse_request_label(request_segment)?;
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
    let commit = crate::net::address::mutate_labeled_address(rtnl, &iface, mutation, label)?;
    notify_address_outcome(netns.clone(), &iface, commit.outcome);
    notify_route_changes(&netns, commit.route_changes);
    Ok(Vec::new())
}

pub(super) fn do_del_addr(
    rtnl: &RtnlGuard,
    request_segment: &AddrSegment,
    netns: Arc<NetNamespace>,
) -> Result<Vec<RouteNlSegment>, SystemError> {
    let label = parse_request_label(request_segment)?;
    let selector = parse_delete_selector(request_segment, label)?;
    let iface = lookup_iface_by_index(request_segment, &netns)?;
    let cidr = resolve_delete_cidr(&iface, selector)?;
    let deleted_label = crate::net::address::address_label(&iface, cidr)?;
    let commit = crate::net::address::mutate_address(rtnl, &iface, AddressMutation::Delete(cidr))?;
    notify_address_outcome_with_label(netns.clone(), &iface, commit.outcome, Some(&deleted_label));
    notify_route_changes(&netns, commit.route_changes);
    Ok(Vec::new())
}

fn notify_route_changes(netns: &Arc<NetNamespace>, changes: crate::net::route::RouteNotifications) {
    for removed in changes.removed {
        if let Err(error) = super::route::notify_route(netns, CSegmentType::DELROUTE, removed) {
            log::warn!(
                "failed to notify address-derived route deletion: {:?}",
                error
            );
        }
    }
    for added in changes.added {
        if let Err(error) = super::route::notify_route(netns, CSegmentType::NEWROUTE, added) {
            log::warn!(
                "failed to notify address-derived route addition: {:?}",
                error
            );
        }
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

fn iface_to_new_addr(request_header: &CMsgSegHdr, iface: &Arc<dyn Iface>) -> Vec<AddrSegment> {
    let mut segments = Vec::new();
    let Ok(ip_addrs) = crate::net::address::address_snapshot(iface) else {
        return segments;
    };

    for (cidr, label) in ip_addrs {
        if let Ok(segment) = addr_to_segment(
            request_header,
            iface,
            cidr,
            CSegmentType::NEWADDR,
            Some(&label),
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
    label_override: Option<&CString>,
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
        scope: if iface.type_() == crate::driver::net::types::InterfaceType::LOOPBACK {
            RtScope::HOST
        } else {
            RtScope::UNIVERSE
        },
        index: NonZeroU32::new(iface.nic_id() as u32),
    };

    let mut attrs = vec![AddrAttr::Address(octets.clone())];
    if matches!(cidr.address(), IpAddress::Ipv4(_)) {
        let label = match label_override {
            Some(label) => label.clone(),
            None => crate::net::address::address_label(iface, cidr)?,
        };
        // Linux keeps an explicit empty label for IPv4 delete matching but
        // inet_fill_ifaddr() omits IFA_LABEL when ifa_label[0] is NUL.
        if !label.as_bytes().is_empty() {
            attrs.push(AddrAttr::Label(label.to_bytes_with_nul().to_vec()));
        }
    }
    attrs.push(AddrAttr::Local(octets));

    Ok(AddrSegment::new(header, addr_message, attrs))
}

pub(in crate::net::socket::netlink::route) fn notify_address_outcome(
    netns: Arc<NetNamespace>,
    iface: &Arc<dyn Iface>,
    outcome: AddressMutationOutcome,
) {
    notify_address_outcome_with_label(netns, iface, outcome, None)
}

pub(super) fn notify_address_change(
    netns: Arc<NetNamespace>,
    iface: &Arc<dyn Iface>,
    cidr: IpCidr,
) {
    notify_one(netns, iface, cidr, CSegmentType::NEWADDR, None);
}

fn notify_address_outcome_with_label(
    netns: Arc<NetNamespace>,
    iface: &Arc<dyn Iface>,
    outcome: AddressMutationOutcome,
    deleted_label: Option<&CString>,
) {
    match outcome {
        AddressMutationOutcome::Added(cidr) | AddressMutationOutcome::Replaced(cidr) => {
            notify_one(netns, iface, cidr, CSegmentType::NEWADDR, None)
        }
        AddressMutationOutcome::Deleted(cidr) => {
            notify_one(netns, iface, cidr, CSegmentType::DELADDR, deleted_label)
        }
    }
}

fn notify_one(
    netns: Arc<NetNamespace>,
    iface: &Arc<dyn Iface>,
    cidr: IpCidr,
    msg_type: CSegmentType,
    label_override: Option<&CString>,
) {
    let header = kernel_notify_header(msg_type);
    let segment = match addr_to_segment(&header, iface, cidr, msg_type, label_override) {
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
