//! Output-route and source-address validation for socket and routing callers.
//!
//! Socket and rtnetlink callers consume this module's result instead of
//! independently interpreting FIB source policy.

use alloc::{collections::BTreeMap, sync::Arc};
use core::cmp::Reverse;

use smoltcp::wire::{IpAddress, IpCidr, Ipv4Address, Ipv6Address, Ipv6AddressExt};
use system_error::SystemError;

use crate::{
    driver::net::{types::InterfaceFlags, Iface},
    process::namespace::net_namespace::NetNamespace,
};

use super::{
    lookup_output_fib_with_source, OutputRouteGuard, RouteLookupResult, RouteSourcePolicy,
    RTN_LOCAL,
};

impl OutputRouteGuard<'_> {
    /// Choose a source for a generated ICMPv6 reply using the pinned FIB
    /// and committed address metadata. Ingress holds the smoltcp locks.
    pub(crate) fn ipv6_routed_icmp_reply_source(
        &self,
        remote: Ipv6Address,
        ingress: u32,
        echo: bool,
        local_destination: Option<Ipv6Address>,
    ) -> Option<Ipv6Address> {
        // A source-specific local route can win for an address configured
        // on another device. Preserve that original destination as the error source.
        if let Some(destination) = local_destination {
            let local_scope = destination.is_unicast_link_local() || destination.is_loopback();
            let owned = self.devices.iter().any(|(&ifindex, iface)| {
                (!local_scope || ifindex == ingress as usize)
                    && iface.common().ipv6_enabled()
                    && iface
                        .common()
                        .address_metadata()
                        .lock()
                        .iter()
                        .any(|entry| {
                            entry.state.is_usable() && entry.cidr.address() == destination.into()
                        })
            });
            if owned {
                return Some(destination);
            }
        }
        let scoped = remote.is_unicast_link_local() || remote.is_loopback();
        let output_device = (scoped || echo).then_some(ingress);
        let preferred_source = self
            .fib
            .lookup_ipv6_output(remote, output_device, None)
            .filter(|route| super::ipv6_route_rejection(route.matched.kind).is_none())
            .and_then(|route| route.matched.preferred_source);
        let now = crate::time::Instant::now().total_micros();
        let remote_scope = ipv6_unicast_scope(remote);
        let mut best = None;
        for (&index, iface) in self.devices.iter() {
            if (scoped && index != ingress as usize)
                || !iface.flags().contains(InterfaceFlags::UP)
                || !iface.common().ipv6_enabled()
            {
                continue;
            }
            for entry in iface.common().address_metadata().lock().iter() {
                let IpCidr::Ipv6(cidr) = entry.cidr else {
                    continue;
                };
                let address = cidr.address();
                let scope = ipv6_unicast_scope(address);
                if !entry.state.is_usable() || !address.x_is_unicast() || scope < remote_scope {
                    continue;
                }
                if preferred_source == Some(address.into()) {
                    return Some(address);
                }
                let preferred = entry
                    .lifetimes
                    .and_then(|lifetime| lifetime.preferred_until_us())
                    .is_none_or(|deadline| deadline > now);
                let score = (
                    address == remote,
                    Reverse(scope),
                    preferred,
                    index == ingress as usize,
                    ipv6_common_prefix(address, remote).min(u32::from(cidr.prefix_len())),
                );
                if best.is_none_or(|(_, previous)| score > previous) {
                    best = Some((address, score));
                }
            }
        }
        best.map(|(address, _)| address)
    }
}

fn ipv6_unicast_scope(address: Ipv6Address) -> u8 {
    let octets = address.octets();
    if address.is_loopback() {
        1
    } else if address.is_unicast_link_local() {
        2
    } else if octets[0] == 0xfe && octets[1] & 0xc0 == 0xc0 {
        5
    } else {
        14
    }
}

pub(super) fn ipv6_common_prefix(left: Ipv6Address, right: Ipv6Address) -> u32 {
    (u128::from_be_bytes(left.octets()) ^ u128::from_be_bytes(right.octets())).leading_zeros()
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ResolvedIpv4Route {
    pub(crate) decision: RouteLookupResult,
    pub(crate) source: IpAddress,
    pub(crate) ip_mtu: usize,
    pub(crate) required_oif: Option<u32>,
}

/// One stable output snapshot for a complete IPv6 datagram. The SocketSet
/// owner and packet source are selected while the route and address view are
/// still held, so the caller does not re-resolve either after OUTPUT.
pub(crate) struct ResolvedIpv6SendRoute {
    pub(crate) decision: super::OutputRouteDecision,
    pub(crate) source: Ipv6Address,
    pub(crate) source_owner: Arc<dyn Iface>,
    pub(crate) route_cookie: u64,
}

pub(crate) enum Ipv6RouteIntent {
    Connect,
    Send(u64),
}

impl ResolvedIpv4Route {
    /// Preserve the same FIB/device snapshot used for source selection when
    /// handing an already-serialized packet to the output owner.
    pub(crate) fn output_decision(self) -> super::OutputRouteDecision {
        super::OutputRouteDecision {
            oif: self.decision.oif,
            required_oif: self.required_oif,
            next_hop: self.decision.next_hop,
            ip_mtu: self.ip_mtu,
            kind: self.decision.matched.kind,
            loopback_anycast: false,
            explicit_ipv6_local: false,
            table: self.decision.table,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Ipv4OutputFlow {
    pub(crate) oif: u32,
    pub(crate) source: IpAddress,
}

/// Resolves an IPv4 FIB winner and source from one topology/FIB/address
/// snapshot. This is the shared authority for socket output and RTM_GETROUTE.
pub(crate) fn resolve_ipv4_route(
    netns: &Arc<NetNamespace>,
    destination: IpAddress,
    required_oif: Option<u32>,
    fixed_source: Option<IpAddress>,
) -> Result<ResolvedIpv4Route, SystemError> {
    if !matches!(destination, IpAddress::Ipv4(_)) {
        return Err(SystemError::EAFNOSUPPORT);
    }

    // Match the data-plane lock order: topology before FIB. Address commits
    // publish their route and address mirror while excluding this FIB reader.
    let devices = netns.device_list();
    let router = netns.router();
    let fib = router.fib.read();
    let decision =
        lookup_output_fib_with_source(&fib, &devices, destination, required_oif, fixed_source)
            .ok_or(SystemError::ENETUNREACH)?;
    let iface = devices
        .get(&(decision.oif as usize))
        .ok_or(SystemError::ENETUNREACH)?;
    if decision.matched.kind != RTN_LOCAL && !iface.flags().contains(InterfaceFlags::UP) {
        return Err(SystemError::ENETDOWN);
    }

    let source_is_local = |source: IpAddress| {
        devices
            .values()
            .any(|candidate| crate::net::address::iface_accepts_local_address(candidate, source))
    };
    let source = if let Some(source) = fixed_source {
        if !matches!(source, IpAddress::Ipv4(_)) || !source_is_local(source) {
            return Err(SystemError::ENETUNREACH);
        }
        source
    } else {
        match decision.source {
            RouteSourcePolicy::Preferred(source) => source_is_local(source)
                .then_some(source)
                .ok_or(SystemError::ENETUNREACH)?,
            RouteSourcePolicy::SelectConfigured | RouteSourcePolicy::AllowUnspecified => {
                let gateway = match decision.matched.gateway {
                    Some(IpAddress::Ipv4(gateway)) => Some(gateway),
                    _ => None,
                };
                let configured = crate::net::address::select_ipv4_source_address(iface, gateway);
                match (configured, decision.source) {
                    (Some(source), _) => source,
                    (None, RouteSourcePolicy::AllowUnspecified) => {
                        IpAddress::Ipv4(Ipv4Address::UNSPECIFIED)
                    }
                    (None, RouteSourcePolicy::SelectConfigured) => {
                        return Err(SystemError::ENETUNREACH)
                    }
                    _ => unreachable!(),
                }
            }
        }
    };

    if decision.matched.kind != RTN_LOCAL
        && matches!(source, IpAddress::Ipv4(addr) if addr.is_loopback())
    {
        super::validate_ipv4_loopback_egress(iface.as_ref())?;
    }

    Ok(ResolvedIpv4Route {
        decision,
        source,
        ip_mtu: iface.mtu(),
        required_oif,
    })
}

pub(crate) fn resolve_ipv4_output_flow(
    netns: &Arc<NetNamespace>,
    destination: IpAddress,
    required_oif: Option<u32>,
    fixed_source: Option<IpAddress>,
) -> Result<Ipv4OutputFlow, SystemError> {
    let resolved = resolve_ipv4_route(netns, destination, required_oif, fixed_source)?;
    Ok(Ipv4OutputFlow {
        oif: resolved.decision.oif,
        source: resolved.source,
    })
}

fn uses_ranked_ipv6_sources(route: RouteLookupResult) -> bool {
    route.matched.is_explicit_ipv6_local() || route.matched.kind == super::RTN_UNICAST
}

fn ipv6_destination_is_scoped(destination: Ipv6Address) -> bool {
    destination.is_loopback() || destination.is_unicast_link_local() || destination.is_multicast()
}

fn select_ranked_ipv6_source(
    devices: &BTreeMap<usize, Arc<dyn Iface>>,
    destination: Ipv6Address,
    oif: u32,
) -> Option<(Ipv6Address, &Arc<dyn Iface>)> {
    let now = smoltcp::time::Instant::from_micros(crate::time::Instant::now().total_micros());
    let candidate_on = |iface: &Arc<dyn Iface>| {
        let index = iface.nic_id();
        let (score, source) = iface.smol_iface().lock().ipv6_source_candidate_at(
            &destination,
            now,
            Some(index == oif as usize),
        )?;
        Some((score, source, index))
    };
    let candidate = if ipv6_destination_is_scoped(destination) {
        candidate_on(devices.get(&(oif as usize))?)
    } else {
        devices
            .values()
            .filter_map(candidate_on)
            .max_by(|left, right| left.0.cmp(&right.0))
    };
    let (_, source, index) = candidate?;
    Some((source, devices.get(&index)?))
}

/// Resolve an IPv6 RTM_GETROUTE output lookup. The reply's preferred source
/// describes the matched route and does not participate in the FIB lookup.
pub(crate) fn resolve_ipv6_route_query(
    netns: &Arc<NetNamespace>,
    destination: Ipv6Address,
    oif: Option<u32>,
    source: Option<Ipv6Address>,
    fib_match: bool,
) -> Result<(RouteLookupResult, Option<IpAddress>), SystemError> {
    let devices = netns.device_list();
    let router = netns.router();
    let fib = router.fib.read();
    let decision = fib.lookup_ipv6_output(destination, oif, source);
    if let Some(rejection) =
        decision.and_then(|route| super::ipv6_route_rejection(route.matched.kind))
    {
        return Err(rejection.error);
    }
    if let Some(oif) = oif {
        let iface = devices.get(&(oif as usize)).ok_or(SystemError::ENODEV)?;
        if !iface.flags().contains(InterfaceFlags::UP) {
            return Err(SystemError::ENETUNREACH);
        }
    }
    let decision = decision.ok_or(SystemError::ENETUNREACH)?;
    let iface = devices
        .get(&(decision.oif as usize))
        .ok_or(SystemError::ENODEV)?;
    let preferred = if fib_match || source.is_some() {
        None
    } else {
        match decision.source {
            RouteSourcePolicy::Preferred(source) => Some(source),
            RouteSourcePolicy::SelectConfigured
                if uses_ranked_ipv6_sources(decision) && !destination.is_unspecified() =>
            {
                select_ranked_ipv6_source(&devices, destination, decision.oif)
                    .map(|(source, _)| source.into())
            }
            _ => crate::net::socket::inet::common::pick_configured_source_addr(
                iface,
                &destination.into(),
            ),
        }
    };
    Ok((decision, preferred))
}

/// Validate an already selected native IPv6 source against the output route.
/// Source selection itself remains in the existing socket source policy.
/// In particular, IPv6 never uses IPv4's explicit-device on-link fallback.
pub(crate) fn resolve_ipv6_output_route(
    netns: &Arc<NetNamespace>,
    destination: IpAddress,
    required_oif: Option<u32>,
    fixed_source: Option<IpAddress>,
) -> Result<super::OutputRouteDecision, SystemError> {
    if !matches!(destination, IpAddress::Ipv6(_)) {
        return Err(SystemError::EAFNOSUPPORT);
    }
    let devices = netns.device_list();
    let router = netns.router();
    let routes = super::lock_output_routes(&router, devices);
    let route = routes
        .lookup_with_source(destination, required_oif, fixed_source)
        .ok_or(SystemError::ENETUNREACH)?
        .require_reachable()?;
    let iface = routes
        .devices
        .get(&(route.oif as usize))
        .ok_or(SystemError::ENETUNREACH)?;
    if route.kind != RTN_LOCAL && !iface.flags().contains(InterfaceFlags::UP) {
        return Err(SystemError::ENETDOWN);
    }
    if let Some(source) = fixed_source {
        if !matches!(source, IpAddress::Ipv6(_))
            || !routes.devices.values().any(|candidate| {
                crate::net::address::iface_accepts_local_address(candidate, source)
            })
        {
            return Err(SystemError::EADDRNOTAVAIL);
        }
        if super::is_ipv6_link_local(source)
            && !crate::net::address::iface_accepts_local_address(iface, source)
        {
            return Err(SystemError::EADDRNOTAVAIL);
        }
    }
    Ok(route)
}

pub(crate) fn resolve_ipv6_send_route(
    netns: &Arc<NetNamespace>,
    destination: IpAddress,
    required_oif: Option<u32>,
    fixed_source: Option<IpAddress>,
) -> Result<ResolvedIpv6SendRoute, SystemError> {
    resolve_ipv6_socket_route(
        netns,
        destination,
        required_oif,
        fixed_source,
        Ipv6RouteIntent::Send(0),
    )
}

pub(crate) fn resolve_ipv6_socket_route(
    netns: &Arc<NetNamespace>,
    destination: IpAddress,
    required_oif: Option<u32>,
    fixed_source: Option<IpAddress>,
    intent: Ipv6RouteIntent,
) -> Result<ResolvedIpv6SendRoute, SystemError> {
    let IpAddress::Ipv6(destination_v6) = destination else {
        return Err(SystemError::EAFNOSUPPORT);
    };
    let devices = netns.device_list();
    let router = netns.router();
    let fib = router.fib.read();
    let fixed_source = match fixed_source {
        Some(IpAddress::Ipv6(source)) if !source.is_unspecified() => Some(source),
        Some(IpAddress::Ipv4(_)) => return Err(SystemError::EADDRNOTAVAIL),
        _ => None,
    };
    let mut source = fixed_source;
    let cookie = match intent {
        Ipv6RouteIntent::Connect => 0,
        Ipv6RouteIntent::Send(cookie) => cookie,
    };
    let mut selected = fib.lookup_ipv6_socket_output(destination_v6, required_oif, source, cookie);
    let first_rejected =
        selected.is_some_and(|route| super::ipv6_route_rejection(route.matched.kind).is_some());
    let route_cookie = if !first_rejected
        && fixed_source.is_none()
        && matches!(intent, Ipv6RouteIntent::Connect)
    {
        selected.map_or(0, |route| {
            fib.ipv6_route_cookie(route, destination_v6, required_oif)
        })
    } else {
        0
    };
    if source.is_none()
        && (first_rejected || selected.is_none() && fib.has_ipv6_source_routes())
        && !destination_v6.is_unspecified()
    {
        // Without an initial route there is no egress or preferred source.
        // Rank addresses once and retry with the selected address, without
        // testing a route for each possible source.
        let scoped_oif = destination_v6
            .is_unicast_link_local()
            .then_some(required_oif)
            .flatten();
        let now = smoltcp::time::Instant::from_micros(crate::time::Instant::now().total_micros());
        let (_, candidate) = devices
            .values()
            .filter(|iface| scoped_oif.is_none_or(|oif| iface.nic_id() == oif as usize))
            .filter_map(|iface| {
                iface
                    .smol_iface()
                    .lock()
                    .ipv6_source_candidate_at(&destination_v6, now, None)
            })
            .max_by(|left, right| left.0.cmp(&right.0))
            .ok_or(SystemError::EADDRNOTAVAIL)?;
        source = Some(candidate);
        selected = fib.lookup_ipv6_output(destination_v6, required_oif, source);
    }
    let selected = selected.ok_or_else(|| {
        // ip6_dst_lookup_tail still attempts automatic source selection
        // after an initial route miss. No candidate takes precedence over
        // that route error; an explicit or available source does not.
        if source.is_none()
            && !devices.values().any(|iface| {
                iface.common().ip_addrs().iter().any(|cidr| {
                    matches!(cidr.address(), IpAddress::Ipv6(source)
                            if !source.is_unspecified() && !source.is_multicast())
                })
            })
        {
            SystemError::EADDRNOTAVAIL
        } else {
            SystemError::ENETUNREACH
        }
    })?;
    if let Some(rejection) = super::ipv6_route_rejection(selected.matched.kind) {
        return Err(rejection.error);
    }
    let egress = devices
        .get(&(selected.oif as usize))
        .ok_or(SystemError::ENETUNREACH)?;
    if selected.matched.kind != RTN_LOCAL && !egress.flags().contains(InterfaceFlags::UP) {
        return Err(SystemError::ENETDOWN);
    }
    let no_source = if destination_v6.is_loopback() {
        SystemError::EADDRNOTAVAIL
    } else {
        SystemError::ENETUNREACH
    };
    let explicit_local = selected.matched.is_explicit_ipv6_local();
    let uses_routed_source_selection = uses_ranked_ipv6_sources(selected);
    let scoped_destination = ipv6_destination_is_scoped(destination_v6);
    let (source, selected_owner) = if let Some(source) = source {
        let local = devices.values().any(|candidate| {
            crate::net::address::iface_accepts_local_address(candidate, source.into())
        });
        if !local
            || super::is_ipv6_link_local(source.into())
                && (!uses_routed_source_selection || scoped_destination)
                && !crate::net::address::iface_accepts_local_address(egress, source.into())
        {
            return Err(SystemError::EADDRNOTAVAIL);
        }
        (source, None)
    } else if uses_routed_source_selection
        && !destination_v6.is_unspecified()
        && matches!(selected.source, RouteSourcePolicy::SelectConfigured)
    {
        let (source, owner) = select_ranked_ipv6_source(&devices, destination_v6, selected.oif)
            .ok_or(SystemError::EADDRNOTAVAIL)?;
        (source, Some(owner.clone()))
    } else {
        let candidate = match selected.source {
            super::RouteSourcePolicy::Preferred(source) => Some(source),
            super::RouteSourcePolicy::SelectConfigured => {
                crate::net::socket::inet::common::pick_configured_source_addr(egress, &destination)
            }
            super::RouteSourcePolicy::AllowUnspecified => Some(Ipv6Address::UNSPECIFIED.into()),
        }
        .ok_or_else(|| no_source.clone())?;
        let IpAddress::Ipv6(candidate) = candidate else {
            return Err(SystemError::EADDRNOTAVAIL);
        };
        if !candidate.is_unspecified()
            && !crate::net::address::iface_has_address(egress, candidate.into())
        {
            return Err(no_source);
        }
        (candidate, None)
    };
    let source_owner = selected_owner
        .or_else(|| {
            devices
                .values()
                .find(|candidate| {
                    crate::net::address::iface_accepts_local_address(candidate, source.into())
                })
                .cloned()
        })
        .ok_or(no_source)?;
    let mtu_oif = selected.output_mtu_oif();
    let ip_mtu = if mtu_oif == selected.oif {
        egress.mtu()
    } else {
        devices
            .get(&(mtu_oif as usize))
            .ok_or(SystemError::ENETUNREACH)?
            .mtu()
    };
    Ok(ResolvedIpv6SendRoute {
        decision: super::OutputRouteDecision {
            oif: selected.oif,
            required_oif,
            next_hop: selected.next_hop,
            ip_mtu,
            kind: selected.matched.kind,
            loopback_anycast: selected.matched.is_loopback_anycast(),
            explicit_ipv6_local: explicit_local,
            table: selected.table,
        },
        source,
        source_owner,
        route_cookie,
    })
}
