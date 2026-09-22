//! Output-route and source-address validation for socket and routing callers.
//!
//! Socket and rtnetlink callers consume this module's result instead of
//! independently interpreting FIB source policy.

use alloc::sync::Arc;

use smoltcp::wire::{IpAddress, Ipv4Address};
use system_error::SystemError;

use crate::{driver::net::types::InterfaceFlags, process::namespace::net_namespace::NetNamespace};

use super::{
    is_limited_broadcast, lookup_output_fib, RouteLookupResult, RouteSourcePolicy, RTN_LOCAL,
};

#[derive(Debug, Clone, Copy)]
pub(crate) struct ResolvedIpv4Route {
    pub(crate) decision: RouteLookupResult,
    pub(crate) source: IpAddress,
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
    // Linux's default route_localnet=0 rejects loopback sources on an
    // explicitly selected non-loopback device, including TCP self-connect.
    if let (Some(IpAddress::Ipv4(source)), Some(oif)) = (fixed_source, required_oif) {
        if source.is_loopback()
            && devices
                .get(&(oif as usize))
                .is_some_and(|iface| !iface.flags().contains(InterfaceFlags::LOOPBACK))
        {
            return Err(SystemError::EINVAL);
        }
    }
    // Linux derives an output device from a fixed local source only for
    // multicast and limited broadcast when no caller supplied an OIF. This is
    // the ip_route_output_key_hash_rcu() compatibility path that lets an
    // address-bound socket send on-link without installing a multicast route.
    // Keep explicit OIF and ordinary weak-host routing independent of source.
    let source_oif = if required_oif.is_none()
        && (destination.is_multicast() || is_limited_broadcast(destination))
    {
        fixed_source.and_then(|source| {
            (!source.is_unspecified()).then_some(())?;
            devices.iter().find_map(|(ifindex, candidate)| {
                crate::net::address::iface_accepts_local_address(candidate, source)
                    .then(|| u32::try_from(*ifindex).ok())
                    .flatten()
            })
        })
    } else {
        None
    };
    let decision = lookup_output_fib(&fib, destination, required_oif.or(source_oif))
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

    Ok(ResolvedIpv4Route { decision, source })
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
