use alloc::{sync::Arc, vec::Vec};

use smoltcp::wire::{EthernetAddress, IpAddress, Ipv4Address, Ipv6Address};
use system_error::SystemError;

use crate::{
    net::{
        neighbor::{
            self, NeighborEntry, NeighborMutationOutcome, NeighborNewFlags, NeighborSnapshot,
            NeighborUpdate, NUD_FAILED,
        },
        rtnl::RtnlGuard,
        socket::netlink::{
            message::segment::{
                header::{CMsgSegHdr, GetRequestFlags, NewRequestFlags, SegHdrCommonFlags},
                CSegmentType,
            },
            route::{
                kern::utils::{
                    finish_response, kernel_notify_header, multicast_notify, RTMGRP_NEIGH,
                },
                message::{
                    attr::neigh::{NeighAttr, NeighAttrClass},
                    segment::{
                        neigh::{NeighSegment, NeighSegmentBody},
                        RouteNlSegment,
                    },
                },
            },
        },
    },
    process::namespace::net_namespace::NetNamespace,
};

const AF_UNSPEC: u8 = 0;
const AF_INET: u8 = 2;
const AF_INET6: u8 = 10;
const NTF_PROXY: u8 = 0x08;

#[derive(Default)]
struct ParsedNeighborAttrs<'a> {
    destination: Option<&'a [u8]>,
    lladdr: Option<&'a [u8]>,
    ifindex: Option<u32>,
    master: Option<u32>,
    flags_ext: Option<u32>,
    protocol: Option<u8>,
}

#[derive(Clone, Copy, Default)]
enum NeighborMasterFilter {
    #[default]
    Any,
    NoMaster,
    Exact(u32),
}

#[derive(Clone, Copy, Default)]
struct NeighborDumpFilter {
    ifindex: Option<u32>,
    master: NeighborMasterFilter,
}

impl NeighborDumpFilter {
    fn from_attrs(attrs: ParsedNeighborAttrs<'_>) -> Self {
        let master = match attrs.master {
            None | Some(0) => NeighborMasterFilter::Any,
            Some(u32::MAX) => NeighborMasterFilter::NoMaster,
            Some(ifindex) => NeighborMasterFilter::Exact(ifindex),
        };
        Self {
            ifindex: attrs.ifindex.filter(|ifindex| *ifindex != 0),
            master,
        }
    }

    fn is_filtered(self) -> bool {
        self.ifindex.is_some() || !matches!(self.master, NeighborMasterFilter::Any)
    }

    fn matches(self, entry: NeighborEntry) -> bool {
        if self.ifindex.is_some_and(|ifindex| entry.ifindex != ifindex) {
            return false;
        }

        // DragonOS does not yet expose a Linux-visible master/upper topology.
        // Every registered interface therefore matches `nomaster`, while an
        // exact master selector matches no configured neighbor. Keeping this
        // decision in the filter object gives the future link implementation
        // one place to replace the topology projection.
        match self.master {
            NeighborMasterFilter::Any | NeighborMasterFilter::NoMaster => true,
            NeighborMasterFilter::Exact(_master_ifindex) => false,
        }
    }
}

impl<'a> ParsedNeighborAttrs<'a> {
    /// Apply Linux's `nda_policy` for operations which call
    /// nlmsg_parse_deprecated(), preserving last-attribute-wins semantics.
    fn from_policy_segment(segment: &'a NeighSegment) -> Result<Self, SystemError> {
        let mut parsed = Self::default();
        for attr in segment.attrs() {
            validate_neighbor_attr(attr)?;
            match attr.class() {
                NeighAttrClass::DST => parsed.destination = Some(attr.payload()),
                NeighAttrClass::LLADDR => parsed.lladdr = Some(attr.payload()),
                NeighAttrClass::IFINDEX => parsed.ifindex = Some(read_u32(attr.payload())?),
                NeighAttrClass::MASTER => parsed.master = Some(read_u32(attr.payload())?),
                NeighAttrClass::FLAGS_EXT => parsed.flags_ext = Some(read_u32(attr.payload())?),
                NeighAttrClass::PROTOCOL => parsed.protocol = attr.payload().first().copied(),
                _ => {}
            }
        }
        Ok(parsed)
    }
}

const MAX_ADDR_LEN: usize = 32;
const NTF_EXT_MANAGED: u32 = 1;

fn validate_neighbor_attr(attr: &NeighAttr) -> Result<(), SystemError> {
    let payload = attr.payload();
    match attr.class() {
        NeighAttrClass::DST | NeighAttrClass::LLADDR if payload.len() > MAX_ADDR_LEN => {
            Err(SystemError::ERANGE)
        }
        NeighAttrClass::CACHEINFO if payload.len() < 16 => Err(SystemError::ERANGE),
        NeighAttrClass::PROBES
        | NeighAttrClass::VNI
        | NeighAttrClass::IFINDEX
        | NeighAttrClass::MASTER
            if payload.len() < size_of::<u32>() =>
        {
            Err(SystemError::ERANGE)
        }
        NeighAttrClass::VLAN | NeighAttrClass::PORT if payload.len() < size_of::<u16>() => {
            Err(SystemError::ERANGE)
        }
        NeighAttrClass::PROTOCOL if payload.is_empty() => Err(SystemError::ERANGE),
        NeighAttrClass::NH_ID if payload.len() != size_of::<u32>() || attr.is_nested() => {
            Err(SystemError::EINVAL)
        }
        NeighAttrClass::FLAGS_EXT => {
            if payload.len() != size_of::<u32>() || attr.is_nested() {
                return Err(SystemError::EINVAL);
            }
            let flags = read_u32(payload)?;
            if flags & !NTF_EXT_MANAGED != 0 {
                Err(SystemError::EINVAL)
            } else {
                Ok(())
            }
        }
        NeighAttrClass::FDB_EXT_ATTRS if !attr.is_nested() => Err(SystemError::EINVAL),
        NeighAttrClass::FDB_EXT_ATTRS
            if !payload.is_empty()
                && payload.len()
                    < size_of::<crate::net::socket::netlink::message::attr::CAttrHeader>() =>
        {
            Err(SystemError::ERANGE)
        }
        NeighAttrClass::NDM_STATE_MASK | NeighAttrClass::NDM_FLAGS_MASK => Err(SystemError::EINVAL),
        _ => Ok(()),
    }
}

fn read_u32(payload: &[u8]) -> Result<u32, SystemError> {
    let bytes = payload.get(..size_of::<u32>()).ok_or(SystemError::EINVAL)?;
    Ok(u32::from_ne_bytes(
        bytes.try_into().map_err(|_| SystemError::EINVAL)?,
    ))
}

pub(super) fn do_get_neigh(
    request: &NeighSegment,
    netns: Arc<NetNamespace>,
) -> Result<Vec<RouteNlSegment>, SystemError> {
    if !GetRequestFlags::from_bits_truncate(request.header().flags).contains(GetRequestFlags::DUMP)
    {
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }

    let family = request.body().family;
    if request.body().flags == NTF_PROXY {
        // Linux selects the separate proxy-neighbor table only for this exact
        // flag value. DragonOS has no proxy table, so never expose normal
        // configured entries as a proxy dump.
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }
    // DragonOS does not expose NETLINK_GET_STRICT_CHK yet. Linux's non-strict
    // dump path discards filter-policy errors and continues unfiltered.
    let attrs = ParsedNeighborAttrs::from_policy_segment(request).unwrap_or_default();
    let filter = NeighborDumpFilter::from_attrs(attrs);
    let filtered = filter.is_filtered();
    let entries = if matches!(family, AF_UNSPEC | AF_INET | AF_INET6) {
        neighbor::snapshot(&netns)?
    } else {
        NeighborSnapshot::empty()
    };
    let mut response = Vec::new();
    response
        .try_reserve_exact(entries.len().saturating_add(1))
        .map_err(|_| SystemError::ENOMEM)?;

    for entry in entries.iter().copied() {
        if !family_matches(family, entry.destination) || !filter.matches(entry) {
            continue;
        }
        response.push(RouteNlSegment::NewNeigh(neigh_to_segment(
            request.header(),
            entry,
            CSegmentType::NEWNEIGH,
            true,
            filtered,
        )?));
    }

    finish_response(request.header(), true, &mut response)?;
    Ok(response)
}

pub(super) fn do_new_neigh(
    rtnl: &RtnlGuard,
    request: &NeighSegment,
    netns: Arc<NetNamespace>,
) -> Result<Vec<RouteNlSegment>, SystemError> {
    let attrs = ParsedNeighborAttrs::from_policy_segment(request)?;
    let destination = attrs.destination.ok_or(SystemError::EINVAL)?;
    let ifindex = parse_ifindex(request.body().ifindex)?;
    let iface = if let Some(ifindex) = ifindex {
        Some(
            netns
                .device_list()
                .get(&(ifindex as usize))
                .cloned()
                .ok_or(SystemError::ENODEV)?,
        )
    } else {
        None
    };
    // For a specified device Linux validates its link address before family;
    // with ifindex zero it has no device length and reaches family first.
    let lladdr = if iface.as_ref().is_some_and(|iface| {
        iface.common().type_() == crate::driver::net::types::InterfaceType::ETHER
    }) {
        attrs.lladdr.map(parse_mac).transpose()?
    } else {
        None
    };
    let destination = parse_ip(destination, request.body().family)?;
    let ifindex = ifindex.ok_or(SystemError::EINVAL)?;
    let iface = iface.ok_or(SystemError::EINVAL)?;
    let nl_flags = NewRequestFlags::from_bits_truncate(request.header().flags);
    let outcome = neighbor::add(
        rtnl,
        &netns,
        &iface,
        NeighborUpdate {
            ifindex,
            destination,
            lladdr,
            state: request.body().state,
            flags: request.body().flags,
            protocol: attrs.protocol.unwrap_or(0),
            flags_ext: attrs.flags_ext.unwrap_or(0),
        },
        NeighborNewFlags {
            replace: nl_flags.contains(NewRequestFlags::REPLACE),
            excl: nl_flags.contains(NewRequestFlags::EXCL),
            create: nl_flags.contains(NewRequestFlags::CREATE),
        },
    )?;

    let changed = match outcome {
        NeighborMutationOutcome::Added(entry) => Some(entry),
        NeighborMutationOutcome::Updated { new, .. } => Some(new),
        NeighborMutationOutcome::Unchanged(_) => None,
    };
    if let Some(entry) = changed {
        notify_entry(&netns, entry, CSegmentType::NEWNEIGH, true);
    }
    Ok(Vec::new())
}

pub(super) fn do_del_neigh(
    rtnl: &RtnlGuard,
    request: &NeighSegment,
    netns: Arc<NetNamespace>,
) -> Result<Vec<RouteNlSegment>, SystemError> {
    // Linux's neigh_delete() uses nlmsg_find_attr(), so unlike NEW and dump,
    // the first duplicate NDA_DST is the selector.
    let destination = request
        .attrs()
        .iter()
        .find_map(|attr| match attr {
            attr if attr.class() == NeighAttrClass::DST => Some(attr.payload()),
            _ => None,
        })
        .ok_or(SystemError::EINVAL)?;
    let ifindex = parse_ifindex(request.body().ifindex)?;
    let iface = if let Some(ifindex) = ifindex {
        Some(
            netns
                .device_list()
                .get(&(ifindex as usize))
                .cloned()
                .ok_or(SystemError::ENODEV)?,
        )
    } else {
        None
    };
    let destination = parse_ip(destination, request.body().family)?;
    if request.body().flags & NTF_PROXY != 0 {
        // NTF_PROXY selects Linux's separate proxy-neighbor table. It must
        // never delete a normal configured entry when that table is absent.
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }
    let iface = iface.ok_or(SystemError::EINVAL)?;
    let removed = neighbor::delete(rtnl, &netns, &iface, destination)?;

    // Linux first invalidates a valid neighbour, then removes it. Both
    // notifications carry NUD_FAILED and omit NDA_LLADDR because FAILED is
    // not a valid neighbour state.
    let failed = NeighborEntry {
        state: NUD_FAILED,
        ..removed
    };
    notify_entry(&netns, failed, CSegmentType::NEWNEIGH, false);
    notify_entry(&netns, failed, CSegmentType::DELNEIGH, false);
    Ok(Vec::new())
}

fn parse_ifindex(ifindex: i32) -> Result<Option<u32>, SystemError> {
    if ifindex == 0 {
        return Ok(None);
    }
    u32::try_from(ifindex)
        .map(Some)
        .map_err(|_| SystemError::ENODEV)
}

fn family_matches(requested: u8, destination: IpAddress) -> bool {
    requested == AF_UNSPEC
        || requested
            == match destination {
                IpAddress::Ipv4(_) => AF_INET,
                IpAddress::Ipv6(_) => AF_INET6,
            }
}

fn parse_ip(bytes: &[u8], family: u8) -> Result<IpAddress, SystemError> {
    match family {
        AF_INET if bytes.len() >= 4 => Ok(IpAddress::Ipv4(Ipv4Address::new(
            bytes[0], bytes[1], bytes[2], bytes[3],
        ))),
        AF_INET6 if bytes.len() >= 16 => Ok(IpAddress::Ipv6(Ipv6Address::new(
            u16::from_be_bytes([bytes[0], bytes[1]]),
            u16::from_be_bytes([bytes[2], bytes[3]]),
            u16::from_be_bytes([bytes[4], bytes[5]]),
            u16::from_be_bytes([bytes[6], bytes[7]]),
            u16::from_be_bytes([bytes[8], bytes[9]]),
            u16::from_be_bytes([bytes[10], bytes[11]]),
            u16::from_be_bytes([bytes[12], bytes[13]]),
            u16::from_be_bytes([bytes[14], bytes[15]]),
        ))),
        AF_INET | AF_INET6 => Err(SystemError::EINVAL),
        _ => Err(SystemError::EAFNOSUPPORT),
    }
}

fn parse_mac(bytes: &[u8]) -> Result<EthernetAddress, SystemError> {
    if bytes.len() < 6 {
        return Err(SystemError::EINVAL);
    }
    let mut mac = [0; 6];
    mac.copy_from_slice(&bytes[..6]);
    Ok(EthernetAddress(mac))
}

fn notify_entry(
    netns: &Arc<NetNamespace>,
    entry: NeighborEntry,
    kind: CSegmentType,
    include_lladdr: bool,
) {
    match neigh_to_segment(
        &kernel_notify_header(kind),
        entry,
        kind,
        include_lladdr,
        false,
    ) {
        Ok(segment) => multicast_notify(
            netns.clone(),
            RTMGRP_NEIGH,
            match kind {
                CSegmentType::DELNEIGH => RouteNlSegment::DelNeigh(segment),
                _ => RouteNlSegment::NewNeigh(segment),
            },
        ),
        Err(error) => log::warn!("failed to allocate neighbour notification: {:?}", error),
    }
}

pub(super) fn notify_removed_entry(netns: &Arc<NetNamespace>, entry: NeighborEntry) {
    notify_entry(netns, entry, CSegmentType::DELNEIGH, true);
}

fn neigh_to_segment(
    request_header: &CMsgSegHdr,
    entry: NeighborEntry,
    kind: CSegmentType,
    include_lladdr: bool,
    dump_filtered: bool,
) -> Result<NeighSegment, SystemError> {
    let mut flags = SegHdrCommonFlags::empty();
    if dump_filtered {
        flags.insert(SegHdrCommonFlags::DUMP_FILTERED);
    }
    let header = CMsgSegHdr {
        len: 0,
        type_: kind as u16,
        flags: flags.bits(),
        seq: request_header.seq,
        pid: request_header.pid,
    };
    let body = NeighSegmentBody {
        family: match entry.destination {
            IpAddress::Ipv4(_) => AF_INET,
            IpAddress::Ipv6(_) => AF_INET6,
        },
        ifindex: i32::try_from(entry.ifindex).map_err(|_| SystemError::EINVAL)?,
        state: entry.state,
        flags: entry.flags,
        kind: entry.kind,
    };
    let mut attrs = Vec::new();
    attrs
        .try_reserve_exact(3 + usize::from(include_lladdr))
        .map_err(|_| SystemError::ENOMEM)?;
    attrs.push(NeighAttr::destination(ip_to_bytes(entry.destination)?));
    if let Some(lladdr) = include_lladdr.then_some(entry.lladdr).flatten() {
        attrs.push(NeighAttr::link_local_address(copy_bytes(
            lladdr.as_bytes(),
        )?));
    }
    // Linux neigh_fill_info() always emits these attributes. Configured
    // non-aging entries have no probe activity or NUD timer history, so a
    // stable all-zero cache record is the honest projection of our model.
    attrs.push(NeighAttr::probes(copy_bytes(&0u32.to_ne_bytes())?));
    attrs.push(NeighAttr::cache_info(copy_bytes(&[0; 16])?));
    Ok(NeighSegment::new(header, body, attrs))
}

fn ip_to_bytes(ip: IpAddress) -> Result<Vec<u8>, SystemError> {
    match ip {
        IpAddress::Ipv4(address) => copy_bytes(&address.octets()),
        IpAddress::Ipv6(address) => copy_bytes(&address.octets()),
    }
}

fn copy_bytes(bytes: &[u8]) -> Result<Vec<u8>, SystemError> {
    let mut copy = Vec::new();
    copy.try_reserve_exact(bytes.len())
        .map_err(|_| SystemError::ENOMEM)?;
    copy.extend_from_slice(bytes);
    Ok(copy)
}
