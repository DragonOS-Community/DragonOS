//! IPv4 output-route and source-address resolution.
//!
//! Socket and rtnetlink callers consume this module's result instead of
//! independently interpreting FIB source policy.

use alloc::sync::Arc;

use smoltcp::wire::{IpAddress, Ipv4Address};
use system_error::SystemError;

use crate::{driver::net::types::InterfaceFlags, process::namespace::net_namespace::NetNamespace};

use super::{lookup_output_fib, RouteLookupResult, RouteSourcePolicy, RTN_LOCAL};

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
    let decision =
        lookup_output_fib(&fib, destination, required_oif).ok_or(SystemError::ENETUNREACH)?;
    let iface = devices
        .get(&(decision.oif as usize))
        .ok_or(SystemError::ENETUNREACH)?;
    if decision.matched.kind != RTN_LOCAL && !iface.flags().contains(InterfaceFlags::UP) {
        return Err(SystemError::ENETDOWN);
    }

    let source_is_local = |source: IpAddress| {
        devices.values().any(|candidate| {
            candidate
                .router_common()
                .ip_addrs
                .read()
                .iter()
                .any(|cidr| cidr.address() == source)
        })
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
