//! Authoritative per-network-namespace route state.
//!
//! The public surface is kept here while storage, transactions, validation and
//! interface/address lifecycle live in cohesive submodules.

mod fib;
mod fib_index;
mod lifecycle;
mod source;
mod transaction;
mod types;
mod validation;

use alloc::{collections::BTreeMap, sync::Arc};

use smoltcp::wire::IpAddress;
use system_error::SystemError;

use crate::libs::rwsem::RwSemReadGuard;
use crate::{
    driver::net::Iface,
    net::{routing::Router, rtnl::RtnlGuard},
    process::namespace::net_namespace::NetNamespace,
};

use fib::FibEditor;
pub(in crate::net) use fib::FibTable;
pub(crate) use lifecycle::{
    commit_addresses, prepare_link_state_change, register_iface, unregister_iface,
    PreparedLinkStateChange,
};
pub(crate) use source::{resolve_ipv4_output_flow, resolve_ipv4_route, Ipv4OutputFlow};
use transaction::{
    prepare_with_devices, projection_for_iface, transact_single, transact_with_devices,
    PreparedTransaction, ProjectionPlan,
};
pub(crate) use types::*;
use validation::{validate_entry, validate_entry_on_iface, validate_gateway_iface};

pub(crate) struct OutputRouteGuard<'a> {
    fib: RwSemReadGuard<'a, FibTable>,
    devices: RwSemReadGuard<'a, BTreeMap<usize, Arc<dyn Iface>>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct OutputRouteDecision {
    pub(crate) oif: u32,
    pub(crate) next_hop: IpAddress,
    pub(crate) ip_mtu: usize,
    pub(crate) kind: u8,
    pub(crate) table: u32,
}

impl OutputRouteGuard<'_> {
    pub(crate) fn lookup(
        &self,
        destination: IpAddress,
        oif: Option<u32>,
    ) -> Option<OutputRouteDecision> {
        let route = lookup_output_fib(&self.fib, destination, oif)?;
        let iface = self.devices.get(&(route.oif as usize))?;
        Some(OutputRouteDecision {
            oif: route.oif,
            next_hop: route.next_hop,
            ip_mtu: iface.mtu(),
            kind: route.matched.kind,
            table: route.table,
        })
    }
}

pub(crate) fn lock_output_routes<'a>(
    router: &'a Router,
    devices: RwSemReadGuard<'a, BTreeMap<usize, Arc<dyn Iface>>>,
) -> OutputRouteGuard<'a> {
    OutputRouteGuard {
        fib: router.fib.read(),
        devices,
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

/// Classifies ingress through Linux's family-specific built-in rule chain.
/// The caller must locally deliver RTN_LOCAL/RTN_BROADCAST results and may
/// forward only RTN_UNICAST results.
pub(crate) fn lookup_ingress(
    netns: &Arc<NetNamespace>,
    destination: IpAddress,
    ingress_oif: u32,
) -> Option<RouteLookupResult> {
    if is_limited_broadcast(destination) {
        return Some(RouteLookupResult::limited_broadcast(ingress_oif));
    }
    netns
        .router()
        .fib
        .read()
        .lookup_ingress(destination, ingress_oif)
}

pub(crate) fn lookup_on_iface(
    netns: &Arc<NetNamespace>,
    destination: IpAddress,
    oif: u32,
) -> Option<RouteLookupResult> {
    lookup_output_fib(&netns.router().fib.read(), destination, Some(oif))
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

pub(crate) fn snapshot(
    netns: &Arc<NetNamespace>,
) -> Result<alloc::vec::Vec<RouteEntry>, SystemError> {
    netns.router().fib.read().snapshot()
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
