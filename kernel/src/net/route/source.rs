//! Output-route and source-address validation for socket and routing callers.
//!
//! Socket and rtnetlink callers consume this module's result instead of
//! independently interpreting FIB source policy.

use alloc::sync::Arc;

use smoltcp::wire::{IpAddress, Ipv4Address, Ipv6Address};
use system_error::SystemError;

use crate::{
    driver::net::{types::InterfaceFlags, Iface},
    process::namespace::net_namespace::NetNamespace,
};

use super::{lookup_output_fib_with_source, RouteLookupResult, RouteSourcePolicy, RTN_LOCAL};

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

    // route_localnet=0 applies to the *selected* output device even when the
    // caller did not bind an OIF. Checking only required_oif lets a 127/8
    // source escape through an ordinary physical default route.
    if matches!(source, IpAddress::Ipv4(addr) if addr.is_loopback())
        && !iface.flags().contains(InterfaceFlags::LOOPBACK)
    {
        return Err(SystemError::EINVAL);
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
        .lookup(destination, required_oif)
        .ok_or(SystemError::ENETUNREACH)?;
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
    let IpAddress::Ipv6(destination_v6) = destination else {
        return Err(SystemError::EAFNOSUPPORT);
    };
    let devices = netns.device_list();
    let router = netns.router();
    let fib = router.fib.read();
    let selected = super::lookup_output_fib(&fib, destination, required_oif)
        .ok_or(SystemError::ENETUNREACH)?;
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
    let source = if let Some(source) = fixed_source {
        let IpAddress::Ipv6(source) = source else {
            return Err(SystemError::EADDRNOTAVAIL);
        };
        let local = devices.values().any(|candidate| {
            crate::net::address::iface_accepts_local_address(candidate, source.into())
        });
        if !local
            || super::is_ipv6_link_local(source.into())
                && !crate::net::address::iface_accepts_local_address(egress, source.into())
        {
            return Err(SystemError::EADDRNOTAVAIL);
        }
        source
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
        candidate
    };
    let source_owner = devices
        .values()
        .find(|candidate| {
            crate::net::address::iface_accepts_local_address(candidate, source.into())
        })
        .cloned()
        .ok_or(no_source)?;
    Ok(ResolvedIpv6SendRoute {
        decision: super::OutputRouteDecision {
            oif: selected.oif,
            required_oif,
            next_hop: selected.next_hop,
            ip_mtu: egress.mtu(),
            kind: selected.matched.kind,
            table: selected.table,
        },
        source,
        source_owner,
    })
}
