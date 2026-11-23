//! IPv6 default-router advertisements are received in the device poll and
//! applied by the namespace poller in RTNL process context.

use alloc::sync::Arc;
use smoltcp::{
    phy::ChecksumCapabilities,
    wire::{
        ipv6::{AddressExt, HEADER_LEN},
        Icmpv6Message, Icmpv6Packet, Icmpv6Repr, IpCidr, Ipv6Address, Ipv6Packet, NdiscRepr,
    },
};
use system_error::SystemError;

use crate::{
    driver::net::{types::InterfaceFlags, Iface, IfaceCommon},
    net::{
        address::ipv6_router_solicitation,
        ipv6_defrag::{fragment_offset, DefragDomain},
        rtnl::{self, RtnlGuard},
        socket::netlink::notify_learned_default,
    },
    process::namespace::net_namespace::NetNamespace,
    time::Instant,
};

use super::{
    fib::{FibEdit, PlannedFibMutation},
    prepare_with_devices, transact_single, LearnedDefaultRoute, RouteEntry, RouterPreference,
    RTN_UNICAST, RT_SCOPE_UNIVERSE, RT_TABLE_MAIN,
};

const ROUTER_PROTOCOL: u8 = 9; // RTPROT_RA
const DEFAULT_ROUTER_METRIC: u32 = 1024;
pub(crate) const ADVERTISEMENT_BATCH: usize = 16;

#[derive(Clone, Copy, Debug)]
pub(crate) struct Advertisement {
    ifindex: u32,
    device_epoch: u64,
    ra_generation: u64,
    solicitation_generation: Option<u64>,
    gateway: Ipv6Address,
    expires_at_us: i64,
    preference: RouterPreference,
    withdraw: bool,
}

#[derive(Debug, Default)]
pub(crate) struct PendingAdvertisements {
    entries: [Option<Advertisement>; ADVERTISEMENT_BATCH],
    head: usize,
    len: usize,
}

impl PendingAdvertisements {
    pub(crate) fn push(&mut self, advertisement: Advertisement) -> bool {
        if self.len == self.entries.len() {
            return false;
        }
        let tail = (self.head + self.len) % self.entries.len();
        self.entries[tail] = Some(advertisement);
        self.len += 1;
        true
    }

    pub(crate) fn pop(&mut self) -> Option<Advertisement> {
        let pending = self.entries[self.head].take()?;
        self.head = (self.head + 1) % self.entries.len();
        self.len -= 1;
        Some(pending)
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.len == 0
    }
}

pub(crate) fn parse(
    bytes: &[u8],
    transport_offset: usize,
    ifindex: u32,
    device_epoch: u64,
    owner: &IfaceCommon,
) -> Option<Advertisement> {
    if bytes.get(transport_offset).copied() != Some(u8::from(Icmpv6Message::RouterAdvert)) {
        return None;
    }
    let solicitation_generation = ipv6_router_solicitation::sent_generation(owner);
    let generation = owner.ra_generation();
    if !owner.accepts_router_advertisements() {
        return None;
    }
    let ip = Ipv6Packet::new_checked(bytes).ok()?;
    let data = ip
        .payload()
        .get(transport_offset.checked_sub(HEADER_LEN)?..)?;
    if ip.hop_limit() != 255
        || !ip.src_addr().is_link_local()
        || !matches!(
            fragment_offset(bytes, DefragDomain::local_input(0, ifindex)),
            Ok(None)
        )
    {
        return None;
    }
    let icmp = Icmpv6Packet::new_checked(data).ok()?;
    if icmp.msg_code() != 0 {
        return None;
    }
    let Icmpv6Repr::Ndisc(NdiscRepr::RouterAdvert {
        router_lifetime, ..
    }) = Icmpv6Repr::parse(
        &ip.src_addr(),
        &ip.dst_addr(),
        &icmp,
        &ChecksumCapabilities::default(),
    )
    .ok()?
    else {
        return None;
    };
    let preference = match (data[5] >> 3) & 3 {
        1 => RouterPreference::High,
        3 => RouterPreference::Low,
        _ => RouterPreference::Medium,
    };
    let lifetime_us = router_lifetime.total_micros() as i64;
    Some(Advertisement {
        ifindex,
        device_epoch,
        ra_generation: generation,
        solicitation_generation,
        gateway: ip.src_addr(),
        expires_at_us: Instant::now().total_micros().saturating_add(lifetime_us),
        preference,
        withdraw: lifetime_us == 0,
    })
}

pub(crate) fn apply(netns: &Arc<NetNamespace>, advertisement: Advertisement) {
    let rtnl = rtnl::lock();
    let Some(iface) = netns
        .device_list()
        .get(&(advertisement.ifindex as usize))
        .cloned()
    else {
        return;
    };
    let common = iface.common();
    if common.namespace_epoch() != advertisement.device_epoch
        || common.ra_generation() != advertisement.ra_generation
        || !iface.flags().contains(InterfaceFlags::UP)
        || !common.accepts_router_advertisements()
    {
        return;
    }
    if let Some(generation) = advertisement.solicitation_generation {
        ipv6_router_solicitation::accept_advertisement(common, generation);
    }
    if common
        .ip_addrs()
        .iter()
        .any(|cidr| cidr.address() == advertisement.gateway.into())
        || (!advertisement.withdraw && advertisement.expires_at_us <= Instant::now().total_micros())
    {
        return;
    }

    let route = RouteEntry {
        destination: IpCidr::new(Ipv6Address::UNSPECIFIED.into(), 0),
        source: None,
        preferred_source: None,
        table: RT_TABLE_MAIN,
        priority: DEFAULT_ROUTER_METRIC,
        tos: 0,
        protocol: ROUTER_PROTOCOL,
        scope: RT_SCOPE_UNIVERSE,
        kind: RTN_UNICAST,
        oif: advertisement.ifindex,
        gateway: Some(advertisement.gateway.into()),
        nexthop_flags: 0,
        learned_default: Some(LearnedDefaultRoute {
            expires_at_us: advertisement.expires_at_us,
            preference: advertisement.preference,
        }),
    };
    if let Err(error) = update(&rtnl, netns, route, advertisement.withdraw) {
        log::debug!("could not apply learned IPv6 default route: {:?}", error);
    }
}

fn update(
    rtnl: &RtnlGuard,
    netns: &Arc<NetNamespace>,
    route: RouteEntry,
    withdraw: bool,
) -> Result<(), SystemError> {
    // RTNL keeps the edit stable while the transaction obtains the device
    // references and prepares its projection. A rejected or duplicate
    // advertisement needs neither a device snapshot nor a FIB writer.
    let edit = netns
        .router()
        .fib
        .read()
        .plan_learned_default(route, withdraw);
    if matches!(edit, FibEdit::None) {
        return Ok(());
    }
    transact_single(rtnl, netns, |_| {
        Ok(PlannedFibMutation { edit, outcome: () })
    })?;
    match edit {
        FibEdit::Insert { route, .. } => notify_learned_default(netns, route, false, false),
        FibEdit::Delete { route, .. } => notify_learned_default(netns, route, true, false),
        FibEdit::Replace { old, new, .. } if old.preference() != new.preference() => {
            notify_learned_default(netns, new, false, true);
        }
        _ => {}
    }
    Ok(())
}

pub(crate) fn next_deadline_us(netns: &NetNamespace) -> Option<i64> {
    netns
        .router()
        .fib
        .read()
        .learned_defaults()
        .filter_map(|route| route.learned_default.map(|learned| learned.expires_at_us))
        .min()
}

/// Linux purges learned defaults across the namespace on a nonzero write to
/// all.forwarding or an interface's forwarding setting. The default template
/// is handled separately by the sysctl caller.
pub(crate) fn set_forwarding(
    rtnl: &RtnlGuard,
    netns: &Arc<NetNamespace>,
    iface: Option<&dyn Iface>,
    value: i32,
) -> Result<(), SystemError> {
    let mut removed = alloc::vec::Vec::new();
    if value != 0 {
        let devices = netns.device_list();
        let should_purge = |route: RouteEntry| {
            route.learned_default.is_some()
                && devices
                    .get(&(route.oif as usize))
                    .is_none_or(|device| device.common().ipv6_accept_ra() != 2)
        };
        if netns
            .router()
            .fib
            .read()
            .learned_defaults()
            .any(should_purge)
        {
            let mut participants = alloc::vec::Vec::new();
            participants
                .try_reserve_exact(devices.len())
                .map_err(|_| SystemError::ENOMEM)?;
            participants.extend(devices.values().cloned());
            let prepared = prepare_with_devices(rtnl, netns, &participants, |fib| {
                fib.remove_where(should_purge)
            })?;
            drop(devices);
            removed = prepared.publish(netns);
        }
    }
    match iface {
        Some(iface) => iface.common().set_ipv6_forwarding(rtnl, value),
        None => netns.set_ipv6_forwarding(rtnl, value),
    }
    for route in removed {
        notify_learned_default(netns, route, true, false);
    }
    Ok(())
}

/// Remove in bounded batches, retaining an earlier retry if projection
/// preparation fails. The FIB and smoltcp already ignore overdue routes.
pub(crate) fn expire(netns: &Arc<NetNamespace>, now_us: i64) -> Option<i64> {
    let rtnl = rtnl::lock();
    for _ in 0..16 {
        let route = netns
            .router()
            .fib
            .read()
            .learned_defaults()
            .find(|route| !route.is_live_at(now_us));
        let Some(route) = route else {
            break;
        };
        if update(&rtnl, netns, route, true).is_err() {
            return Some(now_us.saturating_add(10_000));
        }
    }
    next_deadline_us(netns)
}
