//! Authoritative per-network-namespace route state.
//!
//! Linux route identity is richer than smoltcp's data-plane route.  This
//! module owns the lossless control-plane entries and derives the complete
//! smoltcp projection for every affected interface transactionally.

use alloc::{sync::Arc, vec::Vec};

use smoltcp::{
    iface::Route as SmolRoute,
    wire::{IpAddress, IpCidr, Ipv4Cidr, Ipv6AddressExt, Ipv6Cidr},
};
use system_error::SystemError;

use crate::{
    driver::net::{types::InterfaceFlags, AddressMetadata, Iface},
    net::rtnl::RtnlGuard,
    process::namespace::net_namespace::NetNamespace,
};

pub(crate) const RT_TABLE_MAIN: u32 = 254;
pub(crate) const RT_TABLE_LOCAL: u32 = 255;
pub(crate) const RTPROT_KERNEL: u8 = 2;
pub(crate) const RTPROT_BOOT: u8 = 3;
pub(crate) const RT_SCOPE_UNIVERSE: u8 = 0;
pub(crate) const RT_SCOPE_LINK: u8 = 253;
pub(crate) const RT_SCOPE_HOST: u8 = 254;
pub(crate) const RTN_UNICAST: u8 = 1;
pub(crate) const RTN_LOCAL: u8 = 2;
pub(crate) const RTN_BROADCAST: u8 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct RouteEntry {
    pub destination: IpCidr,
    pub source: Option<IpCidr>,
    pub preferred_source: Option<IpAddress>,
    pub table: u32,
    pub priority: u32,
    pub tos: u8,
    pub protocol: u8,
    pub scope: u8,
    pub kind: u8,
    pub oif: u32,
    pub gateway: Option<IpAddress>,
    pub nexthop_flags: u8,
}

impl RouteEntry {
    fn projection(self) -> SmolRoute {
        SmolRoute {
            cidr: canonical_cidr(self.destination),
            via_router: self.gateway,
            preferred_until: None,
            expires_at: None,
        }
    }

    fn is_projectable(self) -> bool {
        self.source.is_none()
            && (self.table == RT_TABLE_MAIN && self.kind == RTN_UNICAST
                || self.table == RT_TABLE_LOCAL && self.kind == RTN_LOCAL)
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct FibTable {
    entries: Vec<RouteEntry>,
}

impl FibTable {
    pub(crate) fn snapshot(&self) -> Vec<RouteEntry> {
        self.entries.clone()
    }

    fn lookup_in_table(
        &self,
        destination: IpAddress,
        table: u32,
        required_oif: Option<u32>,
        minimum_scope: Option<u8>,
        broadcast: BroadcastLookup,
    ) -> Option<RouteLookupResult> {
        self.entries
            .iter()
            .enumerate()
            .filter(|(_, route)| {
                route.table == table
                    && (route.kind == RTN_UNICAST
                        || route.kind == RTN_LOCAL
                        || route.kind == RTN_BROADCAST && broadcast.matches(route.oif))
                    && route.source.is_none()
                    && route.tos == 0
                    && required_oif.is_none_or(|oif| route.oif == oif)
                    && minimum_scope.is_none_or(|scope| route.scope >= scope)
                    && same_family(route.destination.address(), destination)
                    && route.destination.contains_addr(&destination)
            })
            .max_by(|(left_index, left), (right_index, right)| {
                left.destination
                    .prefix_len()
                    .cmp(&right.destination.prefix_len())
                    .then_with(|| right.priority.cmp(&left.priority))
                    .then_with(|| right_index.cmp(left_index))
            })
            .map(|(_, route)| RouteLookupResult {
                oif: route.oif,
                next_hop: route.gateway.unwrap_or(destination),
                preferred_source: route.preferred_source,
                table: route.table,
                matched: *route,
            })
    }

    fn resolve_gateway(
        &self,
        destination: IpAddress,
        table: u32,
        required_oif: Option<u32>,
        minimum_scope: Option<u8>,
    ) -> Option<u32> {
        let winner = self.lookup_in_table(
            destination,
            table,
            required_oif,
            minimum_scope,
            BroadcastLookup::Exclude,
        )?;
        if winner.matched.gateway.is_some()
            || (is_ipv4(destination) && winner.matched.scope < RT_SCOPE_LINK)
        {
            return None;
        }
        Some(winner.oif)
    }
}

#[derive(Clone, Copy)]
enum BroadcastLookup {
    Exclude,
    Any,
    OnIface(u32),
}

impl BroadcastLookup {
    fn matches(self, oif: u32) -> bool {
        match self {
            Self::Exclude => false,
            Self::Any => true,
            Self::OnIface(required) => required == oif,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct RouteLookupResult {
    pub oif: u32,
    pub next_hop: IpAddress,
    pub preferred_source: Option<IpAddress>,
    pub table: u32,
    pub matched: RouteEntry,
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct RouteNewFlags {
    pub replace: bool,
    pub excl: bool,
    pub create: bool,
    pub append: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RouteMutationOutcome {
    Added { route: RouteEntry, appended: bool },
    Replaced { old: RouteEntry, new: RouteEntry },
    Unchanged(RouteEntry),
}

#[derive(Debug, Default)]
pub(crate) struct RouteChanges {
    pub removed: Vec<RouteEntry>,
    pub added: Vec<RouteEntry>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct RouteDeleteSelector {
    pub destination: IpCidr,
    pub table: u32,
    pub priority: Option<u32>,
    pub tos: Option<u8>,
    pub protocol: Option<u8>,
    pub scope: Option<u8>,
    pub kind: Option<u8>,
    pub oif: Option<u32>,
    /// Distinguishes an omitted gateway selector from an explicitly supplied
    /// zero IPv6 gateway, which selects a direct route on Linux.
    pub gateway_specified: bool,
    pub gateway: Option<IpAddress>,
    pub preferred_source: Option<IpAddress>,
}

pub(crate) fn add_route(
    rtnl: &RtnlGuard,
    netns: &Arc<NetNamespace>,
    route: RouteEntry,
    flags: RouteNewFlags,
) -> Result<RouteMutationOutcome, SystemError> {
    validate_entry(netns, route)?;
    transact(rtnl, netns, |candidate| {
        let outcome = insert(candidate, route, flags)?;
        let impact = ProjectionImpact::from_mutation(outcome);
        Ok((outcome, impact))
    })
}

pub(crate) fn delete_route(
    rtnl: &RtnlGuard,
    netns: &Arc<NetNamespace>,
    selector: RouteDeleteSelector,
) -> Result<RouteEntry, SystemError> {
    transact(rtnl, netns, |candidate| {
        let index = candidate
            .entries
            .iter()
            .position(|route| delete_matches(*route, selector))
            .ok_or(SystemError::ESRCH)?;
        let removed = candidate.entries.remove(index);
        Ok((removed, ProjectionImpact::one(removed.oif)))
    })
}

pub(crate) fn lookup(
    netns: &Arc<NetNamespace>,
    destination: IpAddress,
) -> Option<RouteLookupResult> {
    let router = netns.router();
    let fib = router.fib.read();
    fib.lookup_in_table(
        destination,
        RT_TABLE_LOCAL,
        None,
        None,
        BroadcastLookup::Any,
    )
    .or_else(|| {
        fib.lookup_in_table(
            destination,
            RT_TABLE_MAIN,
            None,
            None,
            BroadcastLookup::Exclude,
        )
    })
}

/// Classifies an ingress destination through Linux's local-before-main rule.
/// The caller must locally deliver RTN_LOCAL/RTN_BROADCAST results and may
/// forward only RTN_UNICAST results.
pub(crate) fn lookup_ingress(
    netns: &Arc<NetNamespace>,
    destination: IpAddress,
) -> Option<RouteLookupResult> {
    let router = netns.router();
    let fib = router.fib.read();
    fib.lookup_in_table(
        destination,
        RT_TABLE_LOCAL,
        None,
        None,
        BroadcastLookup::Any,
    )
    .or_else(|| {
        fib.lookup_in_table(
            destination,
            RT_TABLE_MAIN,
            None,
            None,
            BroadcastLookup::Exclude,
        )
    })
}

pub(crate) fn lookup_on_iface(
    netns: &Arc<NetNamespace>,
    destination: IpAddress,
    oif: u32,
) -> Option<RouteLookupResult> {
    let router = netns.router();
    let fib = router.fib.read();
    fib.lookup_in_table(
        destination,
        RT_TABLE_LOCAL,
        Some(oif),
        None,
        BroadcastLookup::OnIface(oif),
    )
    .or_else(|| {
        fib.lookup_in_table(
            destination,
            RT_TABLE_MAIN,
            Some(oif),
            None,
            BroadcastLookup::Exclude,
        )
    })
}

pub(crate) fn snapshot(netns: &Arc<NetNamespace>) -> Vec<RouteEntry> {
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
    // IPv6 link-local nexthops carry a zone in their explicit output device.
    // Linux does not infer that device through a global FIB lookup.
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
    let in_local = fib.resolve_gateway(gateway, RT_TABLE_LOCAL, requested_oif, minimum_scope);
    let in_requested = (table != RT_TABLE_MAIN)
        .then(|| fib.resolve_gateway(gateway, table, requested_oif, minimum_scope))
        .flatten();
    let oif = in_local
        .or(in_requested)
        .or_else(|| fib.resolve_gateway(gateway, RT_TABLE_MAIN, requested_oif, minimum_scope))
        .ok_or(if is_ipv4(gateway) {
            SystemError::ENETUNREACH
        } else {
            SystemError::EHOSTUNREACH
        })?;
    drop(fib);
    validate_gateway_iface(netns, gateway, oif, false)
}

/// Imports construction-time addresses and routes before an interface becomes
/// visible.  From this point onward the FIB owns the complete projection.
pub(crate) fn register_iface(
    rtnl: &RtnlGuard,
    netns: &Arc<NetNamespace>,
    iface: &Arc<dyn Iface>,
    devices: &[Arc<dyn Iface>],
) -> Result<(), SystemError> {
    let addresses = iface.common().ip_addrs().to_vec();
    let staged = iface.common().take_bootstrap_routes();
    iface
        .smol_iface()
        .lock()
        .set_route_table_includes_connected_prefixes(true);
    let result = transact_with_devices(rtnl, netns, devices, |candidate| {
        for route in derived_address_entries(iface, &addresses) {
            insert_internal(candidate, route);
        }
        for route in staged.iter().copied() {
            let route = RouteEntry {
                destination: canonical_cidr(route.destination),
                source: route.source.map(canonical_cidr),
                preferred_source: route.preferred_source,
                table: route.table,
                priority: route.priority,
                tos: route.tos,
                protocol: route.protocol,
                scope: route.scope,
                kind: route.kind,
                oif: route.oif,
                gateway: route.gateway,
                nexthop_flags: route.nexthop_flags,
            };
            validate_entry_on_iface(iface, route)?;
            insert_internal(candidate, route);
        }
        Ok(((), ProjectionImpact::one(iface.nic_id() as u32)))
    });
    if let Err(error) = result {
        iface.common().restore_bootstrap_routes(staged);
        iface
            .smol_iface()
            .lock()
            .set_route_table_includes_connected_prefixes(false);
        return Err(error);
    }
    Ok(())
}

pub(crate) fn unregister_iface(
    rtnl: &RtnlGuard,
    netns: &Arc<NetNamespace>,
    ifindex: u32,
    devices: &[Arc<dyn Iface>],
) -> Result<(), SystemError> {
    transact_with_devices(rtnl, netns, devices, |candidate| {
        candidate.entries.retain(|route| route.oif != ifindex);
        Ok(((), ProjectionImpact::one(ifindex)))
    })
}

/// Applies Linux's IPv4 device-state FIB lifecycle after an administrative
/// UP transition.  A DOWN event removes non-local routes through the device;
/// bringing it back UP recreates only address-derived connected routes, not
/// administrator-created routes that Linux also discards on NETDEV_DOWN.
pub(crate) fn link_state_changed(
    rtnl: &RtnlGuard,
    netns: &Arc<NetNamespace>,
    iface: &Arc<dyn Iface>,
    is_up: bool,
) -> Result<RouteChanges, SystemError> {
    let devices: Vec<Arc<dyn Iface>> = netns.device_list().values().cloned().collect();
    let ifindex = iface.nic_id() as u32;
    transact_with_devices(rtnl, netns, &devices, |candidate| {
        let before = candidate.entries.clone();
        if is_up {
            for entry in derived_address_entries(iface, &iface.common().ip_addrs()) {
                if is_ipv4(entry.destination.address())
                    && (entry.table == RT_TABLE_MAIN || entry.kind == RTN_BROADCAST)
                {
                    insert_internal(candidate, entry);
                }
            }
        } else {
            candidate.entries.retain(|entry| {
                entry.oif != ifindex
                    || !is_ipv4(entry.destination.address())
                    || entry.table == RT_TABLE_LOCAL
                        && entry.kind == RTN_LOCAL
                        && entry.scope == RT_SCOPE_HOST
            });
        }
        Ok((
            diff_routes(&before, &candidate.entries),
            ProjectionImpact::one(ifindex),
        ))
    })
}

/// Publishes one interface's address list and derived FIB projection as one
/// non-fallible control-plane commit after all candidate work succeeds.
pub(crate) fn commit_addresses(
    _rtnl: &RtnlGuard,
    netns: &Arc<NetNamespace>,
    iface: &Arc<dyn Iface>,
    before: &[IpCidr],
    after: &[IpCidr],
    metadata: Vec<AddressMetadata>,
    deleted_address: Option<IpAddress>,
) -> Result<RouteChanges, SystemError> {
    let ifindex = iface.nic_id() as u32;
    let router = netns.router();
    let fib_before = router.fib.read().clone();
    let mut candidate = fib_before.clone();
    let before_routes = derived_address_entries(iface, before);
    let after_routes = derived_address_entries(iface, after);
    let mut changed_existing_prefixes = Vec::new();
    let mut silently_removed = Vec::new();
    let mut silently_added = Vec::new();

    for old in before_routes.iter().copied() {
        if after_routes.contains(&old) {
            continue;
        }
        if let Some(index) = candidate.entries.iter().position(|entry| *entry == old) {
            candidate.entries.remove(index);
            if after_routes.iter().any(|new| same_derived_slot(*new, old)) {
                changed_existing_prefixes.push(old);
            }
        }
    }
    for new in after_routes.iter().copied() {
        let prefix_existed = before_routes.iter().any(|old| same_derived_slot(*old, new));
        if !candidate.entries.contains(&new)
            && (!prefix_existed
                || changed_existing_prefixes
                    .iter()
                    .any(|old| same_derived_slot(*old, new)))
        {
            insert_internal(&mut candidate, new);
        }
    }
    if let Some(deleted) = deleted_address {
        let mut index = 0;
        while index < candidate.entries.len() {
            let entry = candidate.entries[index];
            if entry.preferred_source == Some(deleted)
                && !(entry.oif == ifindex && after_routes.contains(&entry))
            {
                // Cross-OIF preferred sources are rejected at route creation.
                // Keep that capability boundary executable so widening it
                // cannot silently invalidate this target-OIF-only commit.
                if entry.oif != ifindex {
                    return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
                }
                if is_ipv4(deleted) {
                    candidate.entries.remove(index);
                    continue;
                }
                let old = candidate.entries[index];
                candidate.entries[index].preferred_source = None;
                silently_removed.push(old);
                silently_added.push(candidate.entries[index]);
            }
            index += 1;
        }
    }

    let mut projection = projection_for_iface(&candidate, ifindex);
    let mut address_capacity = 0;
    iface
        .smol_iface()
        .lock()
        .update_ip_addrs(|addresses| address_capacity = addresses.capacity());
    if after.len() > address_capacity {
        return Err(SystemError::ENOSPC);
    }

    let mut changes = diff_routes(&fib_before.entries, &candidate.entries);
    // Linux fib6_remove_prefsrc() updates surviving static routes in place and
    // does not announce a synthetic delete/add pair. The mutation loop records
    // those exact transitions so filtering stays O(N log N), even for a large
    // dynamically-sized FIB.
    silently_removed.sort_unstable();
    silently_added.sort_unstable();
    changes
        .removed
        .retain(|route| silently_removed.binary_search(route).is_err());
    changes
        .added
        .retain(|route| silently_added.binary_search(route).is_err());
    let mut current = router.fib.write();
    debug_assert_eq!(*current, fib_before);
    let mut smol_iface = iface.smol_iface().lock();
    smol_iface.update_ip_addrs(|addresses| {
        addresses.clear();
        for cidr in after.iter().copied() {
            addresses
                .push(cidr)
                .expect("address candidate was capacity-checked");
        }
    });
    smol_iface.routes_mut().update(|routes| {
        core::mem::swap(routes, &mut projection);
    });
    drop(smol_iface);
    *iface.common().address_metadata().lock() = metadata;
    let mut mirror = iface.router_common().ip_addrs.write();
    mirror.clear();
    mirror.extend_from_slice(after);
    drop(mirror);
    *current = candidate;
    Ok(changes)
}

fn same_derived_slot(left: RouteEntry, right: RouteEntry) -> bool {
    left.destination == right.destination
        && left.table == right.table
        && left.kind == right.kind
        && left.oif == right.oif
}

/// The data-plane projections that may change in one FIB transaction.
///
/// Keeping this alongside the mutation result makes projection invalidation
/// explicit: adding a new route kind or mutation path cannot silently fall
/// back to rebuilding every interface.
#[derive(Debug)]
struct ProjectionImpact {
    oifs: Vec<u32>,
}

impl ProjectionImpact {
    fn one(oif: u32) -> Self {
        let mut impact = Self { oifs: Vec::new() };
        impact.include(oif);
        impact
    }

    fn include(&mut self, oif: u32) {
        if !self.oifs.contains(&oif) {
            self.oifs.push(oif);
        }
    }

    fn from_mutation(outcome: RouteMutationOutcome) -> Self {
        let mut impact = Self { oifs: Vec::new() };
        match outcome {
            RouteMutationOutcome::Added { route, .. } | RouteMutationOutcome::Unchanged(route) => {
                impact.include(route.oif)
            }
            RouteMutationOutcome::Replaced { old, new } => {
                impact.include(old.oif);
                impact.include(new.oif);
            }
        }
        impact
    }
}

fn transact<T>(
    rtnl: &RtnlGuard,
    netns: &Arc<NetNamespace>,
    mutate: impl FnOnce(&mut FibTable) -> Result<(T, ProjectionImpact), SystemError>,
) -> Result<T, SystemError> {
    // Topology is snapshotted before the FIB lock. Runtime mutations hold
    // RTNL, so this list remains stable for the complete transaction while
    // lookup paths never hold both locks at once.
    let devices: Vec<Arc<dyn Iface>> = netns.device_list().values().cloned().collect();
    transact_with_devices(rtnl, netns, &devices, mutate)
}

fn transact_with_devices<T>(
    _rtnl: &RtnlGuard,
    netns: &Arc<NetNamespace>,
    devices: &[Arc<dyn Iface>],
    mutate: impl FnOnce(&mut FibTable) -> Result<(T, ProjectionImpact), SystemError>,
) -> Result<T, SystemError> {
    let router = netns.router();
    // RTNL serializes every caller. Build the candidate and its potentially
    // expensive projections without blocking concurrent FIB readers.
    let before = router.fib.read().clone();
    let mut candidate = before.clone();
    let (outcome, impact) = mutate(&mut candidate)?;

    let mut plans = Vec::new();
    for iface in devices
        .iter()
        .filter(|iface| impact.oifs.contains(&(iface.nic_id() as u32)))
    {
        let projection = projection_for_iface(&candidate, iface.nic_id() as u32);
        if projections_equal(
            &projection,
            &projection_for_iface(&before, iface.nic_id() as u32),
        ) {
            continue;
        }
        plans.push((iface.clone(), projection));
    }
    plans.sort_by_key(|(iface, _)| iface.nic_id());

    // Publish every prepared projection and then the authoritative FIB while
    // RTNL excludes writers. Publication cannot fail; an in-flight data-plane
    // lookup may finish with a decision selected before this commit; consumers
    // therefore promise route-level consistency, not a global cache barrier.
    let mut current = router.fib.write();
    debug_assert_eq!(*current, before);
    for (iface, projection) in plans {
        replace_projection(&iface, projection);
    }
    *current = candidate;
    Ok(outcome)
}

fn insert(
    fib: &mut FibTable,
    route: RouteEntry,
    flags: RouteNewFlags,
) -> Result<RouteMutationOutcome, SystemError> {
    let group: Vec<usize> = fib
        .entries
        .iter()
        .enumerate()
        .filter_map(|(index, existing)| conflict_group(*existing, route).then_some(index))
        .collect();
    let exact = group
        .iter()
        .copied()
        .find(|index| fib.entries[*index] == route);

    if flags.excl && !group.is_empty() {
        return Err(SystemError::EEXIST);
    }
    if flags.replace {
        if let Some(first) = group.first().copied() {
            if is_ipv4(route.destination.address()) {
                if exact == Some(first) {
                    return Ok(RouteMutationOutcome::Unchanged(route));
                }
                if exact.is_some() {
                    return Err(SystemError::EEXIST);
                }
            }
            let old = core::mem::replace(&mut fib.entries[first], route);
            return Ok(RouteMutationOutcome::Replaced { old, new: route });
        }
        if !flags.create {
            return Err(SystemError::ENOENT);
        }
    } else if exact.is_some() {
        return Err(SystemError::EEXIST);
    }

    if !flags.create {
        return Err(SystemError::ENOENT);
    }
    if !is_ipv4(route.destination.address()) && !group.is_empty() {
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }

    let position = if let Some(last) = group.last().copied() {
        if flags.append {
            last + 1
        } else {
            group[0]
        }
    } else {
        fib.entries
            .iter()
            .position(|existing| {
                same_prefix_domain(*existing, route) && existing.priority > route.priority
            })
            .unwrap_or(fib.entries.len())
    };
    fib.entries.insert(position, route);
    Ok(RouteMutationOutcome::Added {
        route,
        appended: flags.append && !group.is_empty(),
    })
}

fn insert_internal(fib: &mut FibTable, route: RouteEntry) {
    if fib.entries.contains(&route) {
        return;
    }
    let position = fib
        .entries
        .iter()
        .position(|existing| {
            same_prefix_domain(*existing, route) && existing.priority > route.priority
        })
        .unwrap_or(fib.entries.len());
    fib.entries.insert(position, route);
}

fn diff_routes(before: &[RouteEntry], after: &[RouteEntry]) -> RouteChanges {
    let mut before_index = before.to_vec();
    before_index.sort_unstable();
    let mut after_index = after.to_vec();
    after_index.sort_unstable();

    RouteChanges {
        removed: before
            .iter()
            .copied()
            .filter(|entry| after_index.binary_search(entry).is_err())
            .collect(),
        added: after
            .iter()
            .copied()
            .filter(|entry| before_index.binary_search(entry).is_err())
            .collect(),
    }
}

fn validate_entry(netns: &Arc<NetNamespace>, route: RouteEntry) -> Result<(), SystemError> {
    let iface = netns
        .device_list()
        .get(&(route.oif as usize))
        .cloned()
        .ok_or(SystemError::ENODEV)?;
    validate_entry_on_iface(&iface, route)
}

fn validate_entry_on_iface(iface: &Arc<dyn Iface>, route: RouteEntry) -> Result<(), SystemError> {
    if route.source.is_some() {
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }
    if route.kind != RTN_UNICAST {
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }
    if !same_family_option(route.destination.address(), route.gateway)
        || !same_family_option(route.destination.address(), route.preferred_source)
    {
        return Err(SystemError::EINVAL);
    }
    if !is_ipv4(route.destination.address()) && route.tos != 0 {
        return Err(SystemError::EINVAL);
    }
    let expected_oif = u32::try_from(iface.nic_id()).map_err(|_| SystemError::EOVERFLOW)?;
    if route.oif != expected_oif {
        return Err(SystemError::ENODEV);
    }
    if !iface.flags().contains(InterfaceFlags::UP) {
        return Err(SystemError::ENETDOWN);
    }
    // The current socket sets are owned per interface.  Accepting a source
    // from another interface would make transmit succeed while replies are
    // delivered to a different SocketSet.  Keep this capability boundary
    // explicit until transport demultiplexing is raised to the namespace.
    if let Some(source) = route.preferred_source {
        if !iface
            .common()
            .ip_addrs()
            .iter()
            .any(|cidr| cidr.address() == source)
        {
            return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
        }
    }
    if route.nexthop_flags != 0 && route.scope >= RT_SCOPE_LINK {
        return Err(SystemError::EINVAL);
    }
    if route.gateway.is_some_and(|gateway| !gateway.is_unicast()) {
        return Err(SystemError::EINVAL);
    }
    Ok(())
}

fn delete_matches(route: RouteEntry, selector: RouteDeleteSelector) -> bool {
    route.destination == selector.destination
        && route.table == selector.table
        && selector
            .priority
            .is_none_or(|value| route.priority == value)
        && selector.tos.is_none_or(|value| route.tos == value)
        && selector
            .protocol
            .is_none_or(|value| route.protocol == value)
        && selector.scope.is_none_or(|value| route.scope == value)
        && selector.kind.is_none_or(|value| route.kind == value)
        && selector.oif.is_none_or(|value| route.oif == value)
        && (!selector.gateway_specified || route.gateway == selector.gateway)
        && selector
            .preferred_source
            .is_none_or(|value| route.preferred_source == Some(value))
}

fn validate_gateway_iface(
    netns: &Arc<NetNamespace>,
    gateway: IpAddress,
    oif: u32,
    onlink: bool,
) -> Result<u32, SystemError> {
    let iface = netns
        .device_list()
        .get(&(oif as usize))
        .cloned()
        .ok_or(SystemError::ENODEV)?;
    if !is_ipv4(gateway) {
        let gateway_is_local = if is_ipv6_link_local(gateway) {
            iface
                .common()
                .ip_addrs()
                .iter()
                .any(|cidr| cidr.address() == gateway)
        } else {
            netns.device_list().values().any(|candidate| {
                candidate
                    .common()
                    .ip_addrs()
                    .iter()
                    .any(|cidr| cidr.address() == gateway)
            })
        };
        if iface.flags().contains(InterfaceFlags::LOOPBACK) || gateway_is_local {
            return Err(SystemError::EINVAL);
        }
    }
    if let IpAddress::Ipv4(gateway) = gateway {
        if onlink
            && iface
                .common()
                .ip_addrs()
                .iter()
                .any(|cidr| cidr.address() == IpAddress::Ipv4(gateway))
        {
            return Err(SystemError::EINVAL);
        }
        let invalid_endpoint = iface.common().ip_addrs().iter().any(|cidr| {
            let IpCidr::Ipv4(cidr) = cidr else {
                return false;
            };
            cidr.prefix_len() < 31
                && cidr.contains_addr(&gateway)
                && cidr
                    .broadcast()
                    .is_some_and(|broadcast| gateway.octets() == broadcast.octets())
        });
        if invalid_endpoint {
            return Err(SystemError::EINVAL);
        }
    }
    Ok(oif)
}

fn conflict_group(left: RouteEntry, right: RouteEntry) -> bool {
    same_prefix_domain(left, right)
        && left.priority == right.priority
        && (is_ipv4(left.destination.address()) && left.tos == right.tos
            || !is_ipv4(left.destination.address()))
}

fn same_prefix_domain(left: RouteEntry, right: RouteEntry) -> bool {
    left.table == right.table
        && canonical_cidr(left.destination) == canonical_cidr(right.destination)
        && same_family(left.destination.address(), right.destination.address())
}

fn projection_for_iface(fib: &FibTable, ifindex: u32) -> Vec<SmolRoute> {
    let mut candidates: Vec<RouteEntry> = fib
        .entries
        .iter()
        .copied()
        .filter(|route| route.oif == ifindex && route.is_projectable())
        .collect();
    candidates.sort_by_key(|route| {
        (
            canonical_cidr(route.destination),
            u8::from(route.table != RT_TABLE_LOCAL),
            route.priority,
        )
    });

    let mut projection = Vec::new();
    let mut last_cidr = None;
    for route in candidates {
        let cidr = canonical_cidr(route.destination);
        if last_cidr == Some(cidr) {
            continue;
        }
        projection.push(route.projection());
        last_cidr = Some(cidr);
    }
    projection
}

fn projections_equal(left: &[SmolRoute], right: &[SmolRoute]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .all(|(left, right)| left.cidr == right.cidr && left.via_router == right.via_router)
}

fn replace_projection(iface: &Arc<dyn Iface>, mut projection: Vec<SmolRoute>) {
    iface.smol_iface().lock().routes_mut().update(|routes| {
        core::mem::swap(routes, &mut projection);
    });
}

fn derived_address_entries(iface: &Arc<dyn Iface>, addresses: &[IpCidr]) -> Vec<RouteEntry> {
    let mut result = Vec::new();
    for cidr in addresses.iter().copied() {
        for entry in entries_for_address(iface, cidr, primary_for_prefix(addresses, cidr)) {
            if !result.contains(&entry) {
                result.push(entry);
            }
        }
    }
    result
}

fn entries_for_address(
    iface: &Arc<dyn Iface>,
    cidr: IpCidr,
    primary: Option<IpAddress>,
) -> Vec<RouteEntry> {
    let ipv4 = is_ipv4(cidr.address());
    let loopback = iface.flags().contains(InterfaceFlags::LOOPBACK);
    let mut result = Vec::new();
    let connected = RouteEntry {
        destination: canonical_cidr(cidr),
        source: None,
        preferred_source: ipv4.then_some(primary).flatten(),
        table: if ipv4 && loopback {
            RT_TABLE_LOCAL
        } else {
            RT_TABLE_MAIN
        },
        priority: if ipv4 { 0 } else { 256 },
        tos: 0,
        protocol: RTPROT_KERNEL,
        scope: if ipv4 && loopback {
            RT_SCOPE_HOST
        } else if ipv4 {
            RT_SCOPE_LINK
        } else {
            RT_SCOPE_UNIVERSE
        },
        kind: if ipv4 && loopback {
            RTN_LOCAL
        } else {
            RTN_UNICAST
        },
        oif: iface.nic_id() as u32,
        gateway: None,
        nexthop_flags: 0,
    };
    let add_connected = match cidr {
        IpCidr::Ipv4(cidr) => {
            let octets = cidr.network().address().octets();
            (loopback || cidr.prefix_len() < 32)
                && octets[0] != 0
                && (loopback || iface.flags().contains(InterfaceFlags::UP))
        }
        IpCidr::Ipv6(_) => true,
    };
    if add_connected {
        result.push(connected);
    }

    let local = RouteEntry {
        destination: host_cidr(cidr.address()),
        preferred_source: ipv4.then_some(primary).flatten(),
        table: RT_TABLE_LOCAL,
        priority: 0,
        protocol: RTPROT_KERNEL,
        scope: if ipv4 {
            RT_SCOPE_HOST
        } else {
            RT_SCOPE_UNIVERSE
        },
        kind: RTN_LOCAL,
        ..connected
    };
    if !result.contains(&local) {
        result.push(local);
    }

    if let IpCidr::Ipv4(cidr) = cidr {
        if cidr.prefix_len() < 31
            && cidr.network().address().octets()[0] != 0
            && iface.flags().contains(InterfaceFlags::UP)
        {
            if let Some(broadcast) = cidr.broadcast() {
                result.push(RouteEntry {
                    destination: IpCidr::Ipv4(Ipv4Cidr::new(broadcast, 32)),
                    preferred_source: primary,
                    table: RT_TABLE_LOCAL,
                    priority: 0,
                    protocol: RTPROT_KERNEL,
                    scope: RT_SCOPE_LINK,
                    kind: RTN_BROADCAST,
                    ..connected
                });
            }
        }
    }
    result
}

fn host_cidr(address: IpAddress) -> IpCidr {
    match address {
        IpAddress::Ipv4(address) => IpCidr::Ipv4(Ipv4Cidr::new(address, 32)),
        IpAddress::Ipv6(address) => IpCidr::Ipv6(Ipv6Cidr::new(address, 128)),
    }
}

fn primary_for_prefix(addresses: &[IpCidr], cidr: IpCidr) -> Option<IpAddress> {
    let prefix = canonical_cidr(cidr);
    addresses
        .iter()
        .find(|candidate| canonical_cidr(**candidate) == prefix)
        .map(|candidate| candidate.address())
}

pub(crate) fn canonical_cidr(cidr: IpCidr) -> IpCidr {
    match cidr {
        IpCidr::Ipv4(cidr) => IpCidr::Ipv4(cidr.network()),
        IpCidr::Ipv6(cidr) => IpCidr::Ipv6(Ipv6Cidr::new(
            cidr.address().mask(cidr.prefix_len()).into(),
            cidr.prefix_len(),
        )),
    }
}

fn same_family(left: IpAddress, right: IpAddress) -> bool {
    matches!(
        (left, right),
        (IpAddress::Ipv4(_), IpAddress::Ipv4(_)) | (IpAddress::Ipv6(_), IpAddress::Ipv6(_))
    )
}

fn same_family_option(reference: IpAddress, candidate: Option<IpAddress>) -> bool {
    candidate.is_none_or(|candidate| same_family(reference, candidate))
}

fn is_ipv4(address: IpAddress) -> bool {
    matches!(address, IpAddress::Ipv4(_))
}

fn is_ipv6_link_local(address: IpAddress) -> bool {
    let IpAddress::Ipv6(address) = address else {
        return false;
    };
    let octets = address.octets();
    octets[0] == 0xfe && octets[1] & 0xc0 == 0x80
}
