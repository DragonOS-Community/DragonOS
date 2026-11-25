//! IPv6 Router Advertisements are received in the device poll and
//! applied by the namespace poller in RTNL process context.

use alloc::{sync::Arc, vec::Vec};
use smoltcp::{
    phy::ChecksumCapabilities,
    wire::{
        ipv6::{AddressExt, HEADER_LEN},
        Icmpv6Message, Icmpv6Packet, Icmpv6Repr, IpCidr, Ipv6Address, Ipv6Packet, NdiscOption,
        NdiscOptionRepr, NdiscPrefixInfoFlags, NdiscPrefixInformation, NdiscRepr,
    },
};
use system_error::SystemError;

use crate::{
    driver::net::{types::InterfaceFlags, Iface, IfaceCommon},
    net::{
        address::{ipv6_autoconf::PrefixSetting, ipv6_router_solicitation, ipv6_slaac},
        ipv6_defrag::{fragment_offset, DefragDomain},
        rtnl::{self, RtnlGuard},
        socket::netlink::{has_prefix_listeners, notify_automatic_ipv6_route, notify_ra_prefix},
    },
    process::namespace::net_namespace::NetNamespace,
    time::Instant,
};

use super::{
    canonical_cidr, fib::FibEdit, is_ipv6_link_local, lifecycle::apply_automatic_ipv6_route_edit,
    prepare_with_devices, Ipv6RouteMetadata, LearnedDefaultRoute, RouteEntry, RouterPreference,
    RTN_UNICAST, RTPROT_KERNEL, RT_SCOPE_UNIVERSE, RT_TABLE_MAIN,
};

const ROUTER_PROTOCOL: u8 = 9; // RTPROT_RA
const DEFAULT_ROUTER_METRIC: u32 = 1024;
const PREFIX_ROUTE_METRIC: u32 = 256;
const ROUTER_ADVERTISEMENT_HEADER_LEN: usize = 16;
const MAX_PREFIX_OPTIONS: usize = 256;
const INFINITE_LIFETIME_US: u64 = u32::MAX as u64 * 1_000_000;
pub(crate) const ADVERTISEMENT_BATCH: usize = 16;

#[derive(Debug)]
pub(crate) struct Advertisement {
    ifindex: u32,
    device_epoch: u64,
    ra_generation: u64,
    solicitation_generation: Option<u64>,
    gateway: Ipv6Address,
    expires_at_us: i64,
    preference: RouterPreference,
    withdraw: bool,
    received_at_us: i64,
    prefixes: Vec<PrefixOption>,
}

#[derive(Debug)]
struct PrefixOption {
    info: NdiscPrefixInformation,
    raw_flags: u8,
    notify: bool,
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
    let received_at_us = Instant::now().total_micros();
    Some(Advertisement {
        ifindex,
        device_epoch,
        ra_generation: generation,
        solicitation_generation,
        gateway: ip.src_addr(),
        expires_at_us: received_at_us.saturating_add(lifetime_us),
        preference,
        withdraw: lifetime_us == 0,
        received_at_us,
        prefixes: prefix_information(&data[ROUTER_ADVERTISEMENT_HEADER_LEN..]),
    })
}

fn prefix_information(mut options: &[u8]) -> Vec<PrefixOption> {
    let mut prefixes = Vec::new();
    let mut on_link = 0;
    let mut autoconf = 0;
    let mut notifications = 0;
    // The wire parser has already checked the option stream envelope.
    while let Ok((option, rest)) = NdiscOption::split_first(options) {
        options = rest;
        let Ok(NdiscOptionRepr::PrefixInformation(mut prefix)) = NdiscOptionRepr::parse(&option)
        else {
            continue;
        };
        if prefix.prefix_len > 128
            || prefix.prefix.is_multicast()
            || is_ipv6_link_local(prefix.prefix.into())
            || prefix.preferred_lifetime > prefix.valid_lifetime
        {
            continue;
        }
        // Each consumer keeps its own budget even when a single PIO serves both.
        if on_link == MAX_PREFIX_OPTIONS {
            prefix.flags.remove(NdiscPrefixInfoFlags::ON_LINK);
        }
        if autoconf == MAX_PREFIX_OPTIONS || prefix.prefix_len != 64 {
            prefix.flags.remove(NdiscPrefixInfoFlags::ADDRCONF);
        }
        let notify = notifications < MAX_PREFIX_OPTIONS;
        if prefix.flags.is_empty() && !notify {
            continue;
        }
        if prefixes.try_reserve(1).is_err() {
            break;
        }
        on_link += usize::from(prefix.flags.contains(NdiscPrefixInfoFlags::ON_LINK));
        autoconf += usize::from(prefix.flags.contains(NdiscPrefixInfoFlags::ADDRCONF));
        notifications += usize::from(notify);
        prefixes.push(PrefixOption {
            info: prefix,
            raw_flags: option.raw_prefix_flags(),
            notify,
        });
        if on_link == MAX_PREFIX_OPTIONS
            && autoconf == MAX_PREFIX_OPTIONS
            && notifications == MAX_PREFIX_OPTIONS
        {
            break;
        }
    }
    prefixes
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
    let now_us = Instant::now().total_micros();
    let prefix_settings = common.ipv6_autoconf.lock().prefixes;
    if prefix_settings.get(PrefixSetting::AcceptRaPinfo) != 0 {
        apply_prefix_information(
            &rtnl,
            netns,
            &iface,
            &advertisement,
            now_us,
            prefix_settings.get(PrefixSetting::Autoconf) != 0,
        );
    }
    if common
        .ip_addrs()
        .iter()
        .any(|cidr| cidr.address() == advertisement.gateway.into())
        || (!advertisement.withdraw && advertisement.expires_at_us <= now_us)
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
        ipv6_address_local: false,
        ipv6_metadata: Some(Ipv6RouteMetadata::RaDefault(LearnedDefaultRoute {
            expires_at_us: advertisement.expires_at_us,
            preference: advertisement.preference,
        })),
    };
    if let Err(error) = apply_automatic_ipv6_route_edit(&rtnl, netns, |fib| {
        fib.plan_learned_default(route, advertisement.withdraw)
    }) {
        log::debug!("could not apply learned IPv6 default route: {:?}", error);
    }
}

fn apply_prefix_information(
    rtnl: &RtnlGuard,
    netns: &Arc<NetNamespace>,
    iface: &Arc<dyn Iface>,
    advertisement: &Advertisement,
    now_us: i64,
    autoconf: bool,
) {
    if advertisement.prefixes.is_empty() {
        return;
    }
    let listeners = has_prefix_listeners(netns);
    // RTNL keeps the remaining FIB slots stable across committed edits.
    let mut prefix_slots = 0;
    if advertisement
        .prefixes
        .iter()
        .any(|prefix| prefix.info.flags.contains(NdiscPrefixInfoFlags::ON_LINK))
    {
        prefix_slots = netns.router().fib.read().learned_prefix_slots();
    }

    for option in &advertisement.prefixes {
        let prefix = &option.info;
        if prefix.flags.contains(NdiscPrefixInfoFlags::ON_LINK) {
            match apply_on_link_prefix(rtnl, netns, advertisement, prefix, now_us, prefix_slots > 0)
            {
                Ok(FibEdit::Insert { .. }) => prefix_slots -= 1,
                Ok(FibEdit::Delete { .. }) => prefix_slots += 1,
                Ok(_) => {}
                Err(error) => log::debug!("could not apply IPv6 on-link prefix: {:?}", error),
            }
        }
        if autoconf && option.raw_flags & NdiscPrefixInfoFlags::ADDRCONF.bits() != 0 {
            if prefix.prefix_len != 64 {
                continue;
            }
            if prefix.flags.contains(NdiscPrefixInfoFlags::ADDRCONF) {
                match ipv6_slaac::apply(rtnl, netns, iface, prefix, advertisement.received_at_us) {
                    Ok(true) => {}
                    Ok(false) => continue,
                    Err(error) => {
                        log::debug!("could not apply IPv6 autonomous prefix: {:?}", error);
                        continue;
                    }
                }
            }
        }
        if listeners && option.notify {
            notify_ra_prefix(netns, advertisement.ifindex, prefix, option.raw_flags);
        }
    }
}

fn apply_on_link_prefix(
    rtnl: &RtnlGuard,
    netns: &Arc<NetNamespace>,
    advertisement: &Advertisement,
    prefix: &NdiscPrefixInformation,
    now_us: i64,
    has_slot: bool,
) -> Result<FibEdit, SystemError> {
    let valid_us = prefix.valid_lifetime.total_micros();
    let withdraw = valid_us == 0;
    let expires_at_us = (valid_us != INFINITE_LIFETIME_US)
        .then(|| advertisement.received_at_us.saturating_add(valid_us as i64));
    if !withdraw && expires_at_us.is_some_and(|deadline| deadline <= now_us) {
        return Ok(FibEdit::None);
    }
    let route = RouteEntry {
        destination: canonical_cidr(IpCidr::new(prefix.prefix.into(), prefix.prefix_len)),
        source: None,
        preferred_source: None,
        table: RT_TABLE_MAIN,
        priority: PREFIX_ROUTE_METRIC,
        tos: 0,
        protocol: RTPROT_KERNEL,
        scope: RT_SCOPE_UNIVERSE,
        kind: RTN_UNICAST,
        oif: advertisement.ifindex,
        gateway: None,
        nexthop_flags: 0,
        ipv6_address_local: false,
        ipv6_metadata: Some(Ipv6RouteMetadata::RaOnLinkPrefix { expires_at_us }),
    };
    apply_automatic_ipv6_route_edit(rtnl, netns, |fib| {
        fib.plan_learned_prefix(route, withdraw, has_slot)
    })
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
            route.learned_default().is_some()
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
        notify_automatic_ipv6_route(netns, route, true, false);
    }
    Ok(())
}
