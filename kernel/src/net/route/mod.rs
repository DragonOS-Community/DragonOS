//! Authoritative per-network-namespace route state.
//!
//! The public surface is kept here while storage, transactions, validation and
//! interface/address lifecycle live in cohesive submodules.

mod fib;
mod fib_index;
mod lifecycle;
pub(crate) mod pmtu;
pub(crate) mod ra;
mod source;
mod transaction;
mod types;
mod validation;

use alloc::{collections::BTreeMap, sync::Arc, vec::Vec};

use smoltcp::wire::{IpAddress, IpCidr, Ipv4Address, Ipv6Address, Ipv6AddressExt};
use system_error::SystemError;

use crate::libs::rwsem::RwSemReadGuard;
use crate::{
    driver::net::{types::InterfaceFlags, Iface},
    net::{routing::Router, rtnl::RtnlGuard},
    process::namespace::net_namespace::NetNamespace,
};

use fib::FibEditor;
pub(in crate::net) use fib::FibTable;
pub(in crate::net) use lifecycle::prepare_address_link_change_from;
pub(crate) use lifecycle::{
    commit_addresses, prepare_address_link_change, prepare_link_state_change,
    prepare_unregister_ifaces_from, purge_iface_for_netns_teardown, register_iface,
    AddressLinkChange, PreparedAddressRouteCommit, PreparedIfaceUnregister,
    PreparedLinkStateChange,
};
pub(crate) use source::{
    resolve_ipv4_output_flow, resolve_ipv4_route, Ipv4OutputFlow, ResolvedIpv4Route,
};
pub(crate) use source::{
    resolve_ipv6_output_route, resolve_ipv6_send_route, resolve_ipv6_socket_route, Ipv6RouteIntent,
};
use transaction::{
    prepare_with_devices, prepare_with_devices_from, projection_for_iface, transact_single,
    transact_with_devices, PreparedTransaction, ProjectionPlan,
};
pub(in crate::net) use types::is_ipv6_link_local;
pub(crate) use types::*;
use validation::{validate_entry, validate_entry_on_iface, validate_gateway_iface};

pub(crate) struct OutputRouteGuard<'a> {
    fib: RwSemReadGuard<'a, FibTable>,
    devices: RwSemReadGuard<'a, BTreeMap<usize, Arc<dyn Iface>>>,
    masquerade_addresses: Option<Vec<NatDeviceAddresses>>,
}

struct NatDeviceAddresses {
    ifindex: u32,
    configured: Vec<IpCidr>,
    preferred_deadlines: Vec<(Ipv6Address, i64)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct OutputRouteDecision {
    pub(crate) oif: u32,
    /// The caller's interface constraint, not the interface selected by FIB.
    /// OUTPUT DNAT may select a different OIF only when this is None.
    pub(crate) required_oif: Option<u32>,
    pub(crate) next_hop: IpAddress,
    pub(crate) ip_mtu: usize,
    pub(crate) kind: u8,
    pub(crate) table: u32,
}

impl OutputRouteDecision {
    pub(crate) fn require_reachable(self) -> Result<Self, SystemError> {
        match ipv6_route_rejection(self.kind) {
            Some(rejection) => Err(rejection.error),
            None => Ok(self),
        }
    }
}

impl OutputRouteGuard<'_> {
    /// Capture the configured address order while the FIB read view is held,
    /// before entering any smoltcp interface/socket lock. Address commits
    /// publish their mirror under the FIB writer, so this view and routes are
    /// from the same topology generation. PRE_ROUTING REDIRECT and
    /// POSTROUTING MASQUERADE share this read-only address view.
    pub(crate) fn prepare_masquerade_addresses(&mut self) -> Result<(), SystemError> {
        self.prepare_addresses(None)
    }

    pub(crate) fn prepare_device_addresses(&mut self, ifindex: u32) -> Result<(), SystemError> {
        self.prepare_addresses(Some(ifindex as usize))
    }

    fn prepare_addresses(&mut self, device: Option<usize>) -> Result<(), SystemError> {
        if self.masquerade_addresses.is_some() {
            return Ok(());
        }
        let mut addresses = Vec::new();
        addresses
            .try_reserve_exact(device.map_or(self.devices.len(), |_| 1))
            .map_err(|_| SystemError::ENOMEM)?;
        let entries = device.map_or_else(
            || self.devices.range(..),
            |device| self.devices.range(device..=device),
        );
        for (&index, iface) in entries {
            let oif = u32::try_from(index).map_err(|_| SystemError::EINVAL)?;
            let mirror = iface.common().ip_addrs();
            let mut configured = Vec::new();
            configured
                .try_reserve_exact(mirror.len())
                .map_err(|_| SystemError::ENOMEM)?;
            configured.extend_from_slice(&mirror);
            drop(mirror);
            let mut preferred_deadlines = Vec::new();
            let metadata = iface.common().address_metadata().lock();
            for deadline in crate::net::address::preferred_ipv6_deadlines(&metadata) {
                preferred_deadlines
                    .try_reserve(1)
                    .map_err(|_| SystemError::ENOMEM)?;
                preferred_deadlines.push(deadline);
            }
            addresses.push(NatDeviceAddresses {
                ifindex: oif,
                configured,
                preferred_deadlines,
            });
        }
        self.masquerade_addresses = Some(addresses);
        Ok(())
    }

    /// Linux IPv4 MASQUERADE uses inet_select_addr(out, next_hop, UNIVERSE):
    /// prefer a primary address whose prefix contains the next hop, otherwise
    /// the first primary. IPv6 source selection uses the same supported
    /// scope/exact-match/longest-prefix rules as smoltcp's source selector.
    pub(crate) fn masquerade_address(
        &self,
        oif: u32,
        destination: IpAddress,
        next_hop: IpAddress,
    ) -> Option<IpAddress> {
        let iface = self.devices.get(&(oif as usize))?;
        if !iface
            .flags()
            .contains(crate::driver::net::types::InterfaceFlags::UP)
        {
            return None;
        }
        let addresses = self
            .masquerade_addresses
            .as_ref()?
            .iter()
            .find(|addresses| addresses.ifindex == oif)?;
        match destination {
            IpAddress::Ipv4(_) => {
                let next_hop = match next_hop {
                    IpAddress::Ipv4(address) => address,
                    _ => return None,
                };
                if let Some(source) = select_ipv4_primary(&addresses.configured, next_hop) {
                    return Some(IpAddress::Ipv4(source));
                }
                // inet_select_addr() falls back to a primary on another
                // interface in the same L3 domain. DragonOS has no VRF
                // address domain yet; exclude down/loopback devices here.
                self.masquerade_addresses
                    .as_ref()?
                    .iter()
                    .find_map(|addresses| {
                        (addresses.ifindex != oif
                            && self.devices.get(&(addresses.ifindex as usize)).is_some_and(
                                |device| {
                                    device
                                        .flags()
                                        .contains(crate::driver::net::types::InterfaceFlags::UP)
                                        && !device.flags().contains(
                                            crate::driver::net::types::InterfaceFlags::LOOPBACK,
                                        )
                                },
                            ))
                        .then(|| select_ipv4_primary(&addresses.configured, next_hop))
                        .flatten()
                        .map(IpAddress::Ipv4)
                    })
            }
            IpAddress::Ipv6(destination) => {
                let now_us = crate::time::Instant::now().total_micros();
                select_ipv6_source(&addresses.configured, destination, |source| {
                    addresses
                        .preferred_deadlines
                        .iter()
                        .any(|&(address, deadline)| address == source && deadline <= now_us)
                })
                .map(IpAddress::Ipv6)
            }
        }
    }

    /// PRE_ROUTING REDIRECT chooses an address on the actual ingress device.
    /// Unlike MASQUERADE, it must never fall back to another interface. The
    /// snapshot was prepared before entering the smoltcp interface lock.
    pub(crate) fn redirect_address(
        &self,
        ingress_ifindex: u32,
        destination: IpAddress,
    ) -> Option<IpAddress> {
        self.devices.get(&(ingress_ifindex as usize))?;
        let addresses = self
            .masquerade_addresses
            .as_ref()?
            .iter()
            .find(|addresses| addresses.ifindex == ingress_ifindex)?;
        select_redirect_address(&addresses.configured, destination)
    }

    pub(crate) fn device_mtu(&self, ifindex: u32) -> Option<usize> {
        self.devices
            .get(&(ifindex as usize))
            .map(|iface| iface.mtu())
    }

    pub(crate) fn has_ipv6_source_routes(&self) -> bool {
        self.fib.has_ipv6_source_routes()
    }

    pub(crate) fn ipv4_addr_type(&self, address: Ipv4Address) -> u8 {
        self.fib.ipv4_addr_type(address)
    }

    pub(crate) fn ipv6_addr_type(
        &self,
        address: Ipv6Address,
        source: Option<Ipv6Address>,
        device: Option<u32>,
    ) -> u8 {
        let destination = IpAddress::Ipv6(address);
        let multicast = address.is_multicast();
        let link_scoped =
            is_ipv6_link_local(destination) || (multicast && address.octets()[1] & 0x0f == 2);
        let oif = device.filter(|_| link_scoped);
        let Some(route) = self.fib.lookup_ipv6_output(address, oif, source) else {
            return RTN_UNREACHABLE;
        };
        let route = route.matched;
        if ipv6_route_rejection(route.kind).is_some() {
            return route.kind;
        }
        // As in ipv6_anycast_destination(), an on-link subnet's router
        // anycast address is its prefix address (except on /127 and /128).
        if route.kind == RTN_UNICAST
            && route.gateway.is_none()
            && route.destination.prefix_len() < 127
            && route.destination.address() == destination
        {
            RTN_ANYCAST
        } else if route.kind == RTN_LOCAL {
            RTN_LOCAL
        } else if multicast {
            RTN_MULTICAST
        } else if address.is_unspecified() || address.to_ipv4_mapped().is_some() {
            RTN_UNSPEC
        } else {
            RTN_UNICAST
        }
    }

    pub(crate) fn lookup(
        &self,
        destination: IpAddress,
        oif: Option<u32>,
    ) -> Option<OutputRouteDecision> {
        self.lookup_with_source(destination, oif, None)
    }

    /// Re-resolve an output flow without losing IPv4's source-selected
    /// multicast/broadcast device. The original explicit OIF stays separate
    /// from this destination-dependent choice for subsequent OUTPUT reroutes.
    pub(crate) fn lookup_with_source(
        &self,
        destination: IpAddress,
        oif: Option<u32>,
        source: Option<IpAddress>,
    ) -> Option<OutputRouteDecision> {
        self.lookup_socket_output(destination, oif, source, 0)
    }

    pub(crate) fn lookup_socket_output(
        &self,
        destination: IpAddress,
        oif: Option<u32>,
        source: Option<IpAddress>,
        route_cookie: u64,
    ) -> Option<OutputRouteDecision> {
        let route = if let (IpAddress::Ipv6(destination), Some(IpAddress::Ipv6(source))) =
            (destination, source)
        {
            self.fib.lookup_ipv6_socket_output(
                destination,
                oif,
                (!source.is_unspecified()).then_some(source),
                route_cookie,
            )
        } else {
            lookup_output_fib_with_source(&self.fib, &self.devices, destination, oif, source)
        }?;
        let iface = self.devices.get(&(route.oif as usize))?;
        Some(OutputRouteDecision {
            oif: route.oif,
            required_oif: oif,
            next_hop: route.next_hop,
            ip_mtu: iface.mtu(),
            kind: route.matched.kind,
            table: route.table,
        })
    }

    /// Reuse the poller's pre-acquired FIB view for ingress classification.
    /// A packet hook runs under the smoltcp interface locks and must not
    /// acquire the FIB again in the opposite lock order.
    pub(crate) fn lookup_ingress(
        &self,
        destination: IpAddress,
        ingress_oif: u32,
        source: Option<Ipv6Address>,
    ) -> Option<RouteLookupResult> {
        lookup_ingress_fib(&self.fib, destination, ingress_oif, source)
    }

    pub(crate) fn allows_ipv4_loopback_ingress(&self, ifindex: u32, source: Ipv4Address) -> bool {
        self.devices.get(&(ifindex as usize)).is_some_and(|iface| {
            iface.common().route_localnet_value() != 0
                && !self.devices.values().any(|device| {
                    device
                        .common()
                        .ip_addrs()
                        .iter()
                        .any(|cidr| cidr.address() == IpAddress::Ipv4(source))
                })
        })
    }

    pub(crate) fn validate_ipv4_output_source(
        &self,
        route: OutputRouteDecision,
        source: Ipv4Address,
    ) -> Result<(), SystemError> {
        if route.kind == RTN_LOCAL {
            return Ok(());
        }
        self.validate_ipv4_egress_source(route.oif, source)
    }

    pub(crate) fn validate_ipv4_egress_source(
        &self,
        oif: u32,
        source: Ipv4Address,
    ) -> Result<(), SystemError> {
        if !source.is_loopback() {
            return Ok(());
        }
        let device = self
            .devices
            .get(&(oif as usize))
            .ok_or(SystemError::ENETUNREACH)?;
        validate_ipv4_loopback_egress(device.as_ref())
    }

    pub(crate) fn ingress_device(&self, ifindex: u32) -> Option<Arc<dyn Iface>> {
        self.devices.get(&(ifindex as usize)).cloned()
    }
}

fn validate_ipv4_loopback_egress(iface: &dyn Iface) -> Result<(), SystemError> {
    if !iface.flags().contains(InterfaceFlags::LOOPBACK)
        && iface.common().route_localnet_value() == 0
    {
        return Err(SystemError::EINVAL);
    }
    Ok(())
}

fn select_ipv4_primary(addresses: &[IpCidr], next_hop: Ipv4Address) -> Option<Ipv4Address> {
    let mut fallback = None;
    for (index, item) in addresses.iter().enumerate() {
        let IpCidr::Ipv4(cidr) = item else { continue };
        if cidr.address().is_loopback()
            || addresses[..index].iter().any(
                |prior| matches!(prior, IpCidr::Ipv4(prior) if prior.network() == cidr.network()),
            )
        {
            continue;
        }
        fallback.get_or_insert(cidr.address());
        if cidr.contains_addr(&next_hop) {
            return Some(cidr.address());
        }
    }
    fallback
}

fn select_redirect_address(addresses: &[IpCidr], destination: IpAddress) -> Option<IpAddress> {
    match destination {
        // nf_nat_redirect_ipv4() takes the first ifa_local, even when that
        // address is not the primary source selected for the destination.
        IpAddress::Ipv4(_) => addresses.iter().find_map(|configured| match configured {
            IpCidr::Ipv4(cidr) => Some(IpAddress::Ipv4(cidr.address())),
            IpCidr::Ipv6(_) => None,
        }),
        IpAddress::Ipv6(destination) => {
            let scope = ipv6_redirect_scope(destination);
            addresses.iter().find_map(|configured| {
                let IpCidr::Ipv6(cidr) = configured else {
                    return None;
                };
                let address = cidr.address();
                let octets = address.octets();
                // Linux rejects mapped IPv6 addresses. DragonOS currently
                // has no tentative/optimistic DAD state: configured addresses
                // are immediately usable and no such state is inferred here.
                if octets[..10] == [0; 10] && octets[10..12] == [0xff; 2] {
                    return None;
                }
                (scope == 0 || scope & ipv6_redirect_scope(address) != 0)
                    .then_some(IpAddress::Ipv6(address))
            })
        }
    }
}

/// The low scope bits of Linux __ipv6_addr_type(), used by
/// nf_nat_redirect_ipv6_usable(). A zero scope does not constrain candidates.
fn ipv6_redirect_scope(address: smoltcp::wire::Ipv6Address) -> u8 {
    let octets = address.octets();
    if octets[0] == 0xff {
        return match octets[1] & 0x0f {
            1 => 0x10,
            2 => 0x20,
            5 => 0x40,
            _ => 0,
        };
    }
    if address.is_loopback() {
        return 0x10;
    }
    if octets[0] == 0xfe && octets[1] & 0xc0 == 0x80 {
        return 0x20;
    }
    if octets[0] == 0xfe && octets[1] & 0xc0 == 0xc0 {
        return 0x40;
    }
    if octets[..12] == [0; 12] {
        return 0x80;
    }
    0
}

fn select_ipv6_source(
    addresses: &[IpCidr],
    destination: Ipv6Address,
    deprecated: impl Fn(Ipv6Address) -> bool,
) -> Option<smoltcp::wire::Ipv6Address> {
    let link_scope = destination.is_link_local()
        || destination.is_multicast() && destination.octets()[1] & 0x0f <= 2;
    let mut best = None;
    let mut best_score = (false, 0);
    for item in addresses {
        let IpCidr::Ipv6(cidr) = item else { continue };
        let source = cidr.address();
        if source.is_unspecified() || source.is_multicast() {
            continue;
        }
        if source == destination {
            return Some(source);
        }
        if source.is_link_local() != link_scope || source.is_loopback() {
            continue;
        }
        let mut prefix = 0;
        for (left, right) in source.octets().iter().zip(destination.octets().iter()) {
            prefix += (*left ^ *right).leading_zeros() as usize;
            if left != right {
                break;
            }
        }
        let score = (!deprecated(source), prefix.min(cidr.prefix_len() as usize));
        if best.is_none() || score > best_score {
            best = Some(source);
            best_score = score;
        }
    }
    best
}

#[cfg(test)]
mod masquerade_source_tests {
    use super::{select_ipv4_primary, select_ipv6_source, select_redirect_address};
    use smoltcp::wire::{IpAddress, IpCidr, Ipv4Address, Ipv4Cidr, Ipv6Address, Ipv6Cidr};

    #[test]
    fn ipv4_prefers_primary_on_next_hop_prefix_over_first_and_secondary() {
        let addresses = [
            IpCidr::Ipv4(Ipv4Cidr::new(Ipv4Address::new(192, 0, 2, 10), 24)),
            IpCidr::Ipv4(Ipv4Cidr::new(Ipv4Address::new(198, 51, 100, 20), 24)),
            IpCidr::Ipv4(Ipv4Cidr::new(Ipv4Address::new(198, 51, 100, 21), 24)),
        ];
        assert_eq!(
            select_ipv4_primary(&addresses, Ipv4Address::new(198, 51, 100, 1)),
            Some(Ipv4Address::new(198, 51, 100, 20))
        );
        assert_eq!(
            select_ipv4_primary(&addresses, Ipv4Address::new(203, 0, 113, 1)),
            Some(Ipv4Address::new(192, 0, 2, 10))
        );
    }

    #[test]
    fn ipv6_prefers_matching_global_prefix_over_first_global() {
        let first = Ipv6Address::from([0x20, 1, 0x0d, 0xb8, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
        let second = Ipv6Address::from([0x20, 1, 0x0d, 0xb8, 0, 2, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
        let remote = Ipv6Address::from([0x20, 1, 0x0d, 0xb8, 0, 2, 0, 0, 0, 0, 0, 0, 0, 0, 3]);
        let addresses = [
            IpCidr::Ipv6(Ipv6Cidr::new(first, 64)),
            IpCidr::Ipv6(Ipv6Cidr::new(second, 64)),
        ];
        assert_eq!(
            select_ipv6_source(&addresses, remote, |_| false),
            Some(second)
        );
        assert_eq!(
            select_ipv6_source(&addresses, remote, |address| address == second),
            Some(first)
        );
        assert_eq!(
            select_ipv6_source(&addresses, second, |address| address == second),
            Some(second)
        );
    }

    #[test]
    fn redirect_ipv4_uses_first_ingress_local_address_not_source_selection() {
        let first = Ipv4Address::new(192, 0, 2, 10);
        let second = Ipv4Address::new(198, 51, 100, 20);
        let addresses = [
            IpCidr::Ipv4(Ipv4Cidr::new(first, 24)),
            IpCidr::Ipv4(Ipv4Cidr::new(second, 24)),
        ];
        assert_eq!(
            select_redirect_address(&addresses, IpAddress::Ipv4(second)),
            Some(IpAddress::Ipv4(first))
        );
    }

    #[test]
    fn redirect_ipv6_uses_first_matching_scope_and_skips_mapped() {
        let mapped = Ipv6Address::from([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xff, 1, 2, 3, 4]);
        let global = Ipv6Address::from([0x20, 1, 0x0d, 0xb8, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
        let link = Ipv6Address::from([0xfe, 0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
        let addresses = [
            IpCidr::Ipv6(Ipv6Cidr::new(mapped, 128)),
            IpCidr::Ipv6(Ipv6Cidr::new(global, 64)),
            IpCidr::Ipv6(Ipv6Cidr::new(link, 64)),
        ];
        assert_eq!(
            select_redirect_address(&addresses, IpAddress::Ipv6(link)),
            Some(IpAddress::Ipv6(link))
        );
        // Linux's global destination scope is zero and imposes no scope
        // constraint; it still skips the mapped address.
        assert_eq!(
            select_redirect_address(&addresses, IpAddress::Ipv6(global)),
            Some(IpAddress::Ipv6(global))
        );
        assert_eq!(select_redirect_address(&[], IpAddress::Ipv6(link)), None);
    }
}

pub(crate) fn lock_output_routes<'a>(
    router: &'a Router,
    devices: RwSemReadGuard<'a, BTreeMap<usize, Arc<dyn Iface>>>,
) -> OutputRouteGuard<'a> {
    OutputRouteGuard {
        fib: router.fib.read(),
        devices,
        masquerade_addresses: None,
    }
}

pub(crate) fn add_route(
    rtnl: &RtnlGuard,
    netns: &Arc<NetNamespace>,
    route: RouteEntry,
    flags: RouteNewFlags,
) -> Result<RouteMutationOutcome, SystemError> {
    validate_entry(netns, route)?;
    transact_single(rtnl, netns, |fib| fib.plan_insert(route, flags))
}

pub(crate) fn delete_route(
    rtnl: &RtnlGuard,
    netns: &Arc<NetNamespace>,
    selector: RouteDeleteSelector,
) -> Result<RouteEntry, SystemError> {
    transact_single(rtnl, netns, |fib| fib.plan_delete(selector))
}

pub(crate) fn lookup(
    netns: &Arc<NetNamespace>,
    destination: IpAddress,
) -> Option<RouteLookupResult> {
    lookup_output_fib(&netns.router().fib.read(), destination, None)
}

pub(crate) fn lookup_ipv6_output(
    netns: &Arc<NetNamespace>,
    destination: Ipv6Address,
    oif: Option<u32>,
    source: Option<Ipv6Address>,
) -> Option<RouteLookupResult> {
    netns
        .router()
        .fib
        .read()
        .lookup_ipv6_output(destination, oif, source)
}

pub(crate) fn ipv4_addr_type(netns: &Arc<NetNamespace>, address: Ipv4Address) -> u8 {
    netns.router().fib.read().ipv4_addr_type(address)
}

/// Classifies ingress through Linux's family-specific built-in rule chain.
/// The caller must locally deliver RTN_LOCAL/RTN_BROADCAST results and may
/// forward only RTN_UNICAST results.
pub(crate) fn lookup_ingress(
    netns: &Arc<NetNamespace>,
    destination: IpAddress,
    ingress_oif: u32,
    source: Option<Ipv6Address>,
) -> Option<RouteLookupResult> {
    lookup_ingress_fib(&netns.router().fib.read(), destination, ingress_oif, source)
}

fn lookup_ingress_fib(
    fib: &FibTable,
    destination: IpAddress,
    ingress_oif: u32,
    source: Option<Ipv6Address>,
) -> Option<RouteLookupResult> {
    if is_limited_broadcast(destination) {
        return Some(RouteLookupResult::limited_broadcast(ingress_oif));
    }
    fib.lookup_ingress(destination, ingress_oif, source)
}

/// Resolves the protocol-stack owner that receives packets for an exact local
/// address.
///
/// DragonOS currently keeps one smoltcp `SocketSet` per interface, so socket
/// placement must follow the winning local-table route rather than the output
/// interface. This is the per-interface representation of Linux's netns-wide
/// transport demultiplexing. The address check prevents a stale or synthetic
/// local route from being used as a socket owner.
pub(crate) fn local_address_owner(
    netns: &Arc<NetNamespace>,
    address: IpAddress,
) -> Option<Arc<dyn Iface>> {
    let decision = lookup(netns, address)?;
    if decision.matched.kind != RTN_LOCAL {
        return None;
    }
    let iface = netns.device_list().get(&(decision.oif as usize)).cloned()?;
    crate::net::address::iface_has_address(&iface, address).then_some(iface)
}

/// Keep source-dependent IPv4 device selection shared between initial
/// socket resolution and later queries over an already acquired route view.
fn lookup_output_fib_with_source(
    fib: &FibTable,
    devices: &BTreeMap<usize, Arc<dyn Iface>>,
    destination: IpAddress,
    oif: Option<u32>,
    source: Option<IpAddress>,
) -> Option<RouteLookupResult> {
    if let IpAddress::Ipv6(destination) = destination {
        let source = match source {
            Some(IpAddress::Ipv6(source)) if !source.is_unspecified() => Some(source),
            _ => None,
        };
        return fib.lookup_ipv6_output(destination, oif, source);
    }
    // Linux ip_route_output_key_hash_rcu() uses a fixed local source to
    // select multicast/lbcast output only when there is no explicit OIF.
    // Ordinary unicast keeps weak-host routing independent of source owner.
    let oif = if oif.is_none()
        && matches!(destination, IpAddress::Ipv4(address) if address.is_multicast() || address.is_broadcast())
    {
        match source.filter(|address| !address.is_unspecified()) {
            Some(source) => Some(devices.iter().find_map(|(&ifindex, iface)| {
                crate::net::address::iface_accepts_local_address(iface, source)
                    .then(|| u32::try_from(ifindex).ok())
                    .flatten()
            })?),
            None => None,
        }
    } else {
        oif
    };
    lookup_output_fib(fib, destination, oif)
}

/// Applies the destination-specific output classification shared by immediate
/// socket lookup and deferred namespace-routed output. Keeping this policy at
/// the FIB boundary prevents one output path from retaining a gateway for
/// limited-broadcast or IPv4 multicast traffic.
fn lookup_output_fib(
    fib: &FibTable,
    destination: IpAddress,
    oif: Option<u32>,
) -> Option<RouteLookupResult> {
    if is_limited_broadcast(destination) {
        return match oif {
            Some(oif) => Some(RouteLookupResult::limited_broadcast(oif)),
            None => fib
                .lookup_output(destination)
                .map(RouteLookupResult::into_limited_broadcast),
        };
    }
    let route = match oif {
        Some(oif) => fib.lookup_on_iface(destination, oif),
        None => fib.lookup_output(destination),
    }?;
    if let Some(multicast) = ipv4_multicast(destination) {
        return Some(route.into_multicast(multicast));
    }
    Some(route)
}

pub(crate) struct Ipv4MainRoute {
    pub(crate) entry: RouteEntry,
    order: fib_index::AliasOrder,
}

/// Read-only display order. Neither the route lookup index nor formatted
/// records are copied, and sorting runs after releasing the FIB lock.
pub(crate) fn snapshot_ipv4_main(netns: &NetNamespace) -> Result<Vec<Ipv4MainRoute>, SystemError> {
    use core::cmp::Reverse;

    let mut entries = netns.router().fib.read().snapshot_ipv4_main()?;
    entries.sort_unstable_by_key(|row| {
        let route = row.entry;
        // Linux walks numeric IPv4 trie keys, then the leaf's aliases from
        // shortest suffix (longest prefix), highest DSCP and lowest metric.
        (
            route.destination.address(),
            Reverse(route.destination.prefix_len()),
            Reverse(route.tos),
            row.order,
        )
    });
    Ok(entries)
}

/// One live-dump item, without cloning the FIB or its lookup index.
pub(crate) fn dump_entry(netns: &Arc<NetNamespace>, index: usize) -> Option<RouteEntry> {
    netns.router().fib.read().dump_entry(index)
}

pub(crate) fn resolve_gateway_oif(
    netns: &Arc<NetNamespace>,
    gateway: IpAddress,
    table: u32,
    requested_oif: Option<u32>,
    onlink: bool,
    route_scope: u8,
) -> Result<u32, SystemError> {
    if is_ipv6_link_local(gateway) {
        let oif = requested_oif
            .filter(|oif| *oif != 0)
            .ok_or(SystemError::EINVAL)?;
        return validate_gateway_iface(netns, gateway, oif, onlink);
    }
    if onlink {
        let oif = requested_oif
            .filter(|oif| *oif != 0)
            .ok_or(SystemError::EINVAL);
        return oif.and_then(|oif| validate_gateway_iface(netns, gateway, oif, true));
    }
    let router = netns.router();
    let fib = router.fib.read();
    let minimum_scope =
        is_ipv4(gateway).then_some(route_scope.saturating_add(1).max(RT_SCOPE_LINK));
    let oif = fib
        .resolve_gateway_with_builtin_rules(gateway, table, requested_oif, minimum_scope)
        .ok_or(if is_ipv4(gateway) {
            SystemError::ENETUNREACH
        } else {
            SystemError::EHOSTUNREACH
        })?;
    drop(fib);
    validate_gateway_iface(netns, gateway, oif, false)
}
