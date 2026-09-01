use alloc::vec::Vec;

use smoltcp::wire::IpAddress;
use system_error::SystemError;

use super::{
    is_ipv4, same_family, RouteDeleteSelector, RouteEntry, RouteLookupResult, RouteMutationOutcome,
    RouteNewFlags, RouteNotifications, RouteSourcePolicy, RTN_BROADCAST, RTN_LOCAL, RTN_UNICAST,
    RT_SCOPE_LINK,
};

#[derive(Debug, Default, PartialEq, Eq)]
pub(in crate::net) struct FibTable {
    entries: Vec<RouteEntry>,
}

pub(super) struct FibDelta {
    pub removed: Vec<RouteEntry>,
    pub added: Vec<RouteEntry>,
}

impl FibDelta {
    pub(super) fn into_notifications(self) -> RouteNotifications {
        RouteNotifications {
            removed: self.removed,
            added: self.added,
        }
    }
}

/// The only mutation surface used by ordinary RTNL transactions. Every
/// operation records projection impact next to the state change, so callers
/// cannot add a new mutation path and forget invalidation bookkeeping.
pub(super) struct FibEditor<'a> {
    fib: &'a mut FibTable,
    affected_oifs: Vec<u32>,
}

impl<'a> FibEditor<'a> {
    pub(super) fn new(fib: &'a mut FibTable) -> Self {
        Self {
            fib,
            affected_oifs: Vec::new(),
        }
    }

    fn record(&mut self, oif: u32) -> Result<(), SystemError> {
        if !self.affected_oifs.contains(&oif) {
            self.affected_oifs
                .try_reserve(1)
                .map_err(|_| SystemError::ENOMEM)?;
            self.affected_oifs.push(oif);
        }
        Ok(())
    }

    pub(super) fn insert(
        &mut self,
        route: RouteEntry,
        flags: RouteNewFlags,
    ) -> Result<RouteMutationOutcome, SystemError> {
        let outcome = self.fib.insert(route, flags)?;
        match outcome {
            RouteMutationOutcome::Added { route, .. } | RouteMutationOutcome::Unchanged(route) => {
                self.record(route.oif)?
            }
            RouteMutationOutcome::Replaced { old, new } => {
                self.record(old.oif)?;
                self.record(new.oif)?;
            }
        }
        Ok(outcome)
    }

    pub(super) fn insert_derived(&mut self, route: RouteEntry) -> Result<bool, SystemError> {
        let inserted = self.fib.insert_derived(route)?;
        if inserted {
            self.record(route.oif)?;
        }
        Ok(inserted)
    }

    pub(super) fn delete(
        &mut self,
        selector: RouteDeleteSelector,
    ) -> Result<RouteEntry, SystemError> {
        let removed = self.fib.delete(selector)?;
        self.record(removed.oif)?;
        Ok(removed)
    }

    pub(super) fn remove_where(
        &mut self,
        predicate: impl Fn(RouteEntry) -> bool,
    ) -> Result<(), SystemError> {
        // Record every affected projection before mutating the candidate. If
        // bookkeeping allocation fails, the transaction can still abort with
        // its candidate untouched.
        for index in 0..self.fib.entries.len() {
            let route = self.fib.entries[index];
            if predicate(route) {
                self.record(route.oif)?;
            }
        }
        self.fib.entries.retain(|route| !predicate(*route));
        Ok(())
    }

    pub(super) fn finish(self) -> Vec<u32> {
        self.affected_oifs
    }
}

#[derive(Clone, Copy)]
enum BroadcastLookup {
    Exclude,
    Any,
    OnIface(u32),
}

#[derive(Clone, Copy)]
struct FibLookupKey {
    destination: IpAddress,
    table: u32,
    required_oif: Option<u32>,
    minimum_scope: Option<u8>,
    broadcast: BroadcastLookup,
}

impl FibLookupKey {
    fn output(destination: IpAddress, table: u32) -> Self {
        Self {
            destination,
            table,
            required_oif: None,
            minimum_scope: None,
            broadcast: if table == super::RT_TABLE_LOCAL {
                BroadcastLookup::Any
            } else {
                BroadcastLookup::Exclude
            },
        }
    }

    fn on_iface(destination: IpAddress, table: u32, oif: u32) -> Self {
        Self {
            required_oif: Some(oif),
            broadcast: if table == super::RT_TABLE_LOCAL {
                BroadcastLookup::OnIface(oif)
            } else {
                BroadcastLookup::Exclude
            },
            ..Self::output(destination, table)
        }
    }

    fn ingress_local(destination: IpAddress, _ingress_oif: u32) -> Self {
        Self {
            destination,
            table: super::RT_TABLE_LOCAL,
            required_oif: None,
            minimum_scope: None,
            // Linux's local table is weak-host: a directed broadcast remains
            // local even when it arrived through another interface. Limited
            // broadcast is handled as a transient ingress decision by the
            // route facade and never reaches this FIB lookup.
            broadcast: BroadcastLookup::Any,
        }
    }

    fn gateway(
        destination: IpAddress,
        table: u32,
        required_oif: Option<u32>,
        minimum_scope: Option<u8>,
    ) -> Self {
        Self {
            destination,
            table,
            required_oif,
            minimum_scope,
            broadcast: BroadcastLookup::Exclude,
        }
    }
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

impl FibTable {
    pub(super) fn try_clone(&self) -> Result<Self, SystemError> {
        let mut entries = Vec::new();
        entries
            .try_reserve_exact(self.entries.len())
            .map_err(|_| SystemError::ENOMEM)?;
        entries.extend_from_slice(&self.entries);
        Ok(Self { entries })
    }

    pub(super) fn snapshot(&self) -> Result<Vec<RouteEntry>, SystemError> {
        Ok(self.try_clone()?.entries)
    }

    pub(super) fn entries(&self) -> &[RouteEntry] {
        &self.entries
    }

    fn lookup_key(&self, key: FibLookupKey) -> Option<RouteLookupResult> {
        self.entries
            .iter()
            .enumerate()
            .filter(|(_, route)| {
                route.table == key.table
                    && (route.kind == RTN_UNICAST
                        || route.kind == RTN_LOCAL
                        || route.kind == RTN_BROADCAST && key.broadcast.matches(route.oif))
                    && route.source.is_none()
                    && route.tos == 0
                    && key.required_oif.is_none_or(|oif| route.oif == oif)
                    && key.minimum_scope.is_none_or(|scope| route.scope >= scope)
                    && same_family(route.destination.address(), key.destination)
                    && route.destination.contains_addr(&key.destination)
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
                next_hop: route.gateway.unwrap_or(key.destination),
                source: route
                    .preferred_source
                    .map(RouteSourcePolicy::Preferred)
                    .unwrap_or(RouteSourcePolicy::SelectConfigured),
                table: route.table,
                matched: *route,
            })
    }

    pub(super) fn lookup_output(&self, destination: IpAddress) -> Option<RouteLookupResult> {
        self.lookup_key(FibLookupKey::output(destination, super::RT_TABLE_LOCAL))
            .or_else(|| self.lookup_key(FibLookupKey::output(destination, super::RT_TABLE_MAIN)))
    }

    pub(super) fn lookup_on_iface(
        &self,
        destination: IpAddress,
        oif: u32,
    ) -> Option<RouteLookupResult> {
        self.lookup_key(FibLookupKey::on_iface(
            destination,
            super::RT_TABLE_LOCAL,
            oif,
        ))
        .or_else(|| {
            self.lookup_key(FibLookupKey::on_iface(
                destination,
                super::RT_TABLE_MAIN,
                oif,
            ))
        })
        // Linux IPv4 treats an explicit output interface as an on-link route
        // when the normal FIB lookup misses.  This applies to every unicast
        // destination (including a local address owned by another interface),
        // but IPv6 deliberately has no equivalent fallback.
        .or_else(|| match destination {
            IpAddress::Ipv4(destination) => {
                Some(RouteLookupResult::direct_ipv4_output(destination, oif))
            }
            IpAddress::Ipv6(_) => None,
        })
    }

    pub(super) fn lookup_ingress(
        &self,
        destination: IpAddress,
        ingress_oif: u32,
    ) -> Option<RouteLookupResult> {
        self.lookup_key(FibLookupKey::ingress_local(destination, ingress_oif))
            .or_else(|| self.lookup_key(FibLookupKey::output(destination, super::RT_TABLE_MAIN)))
    }

    pub(super) fn resolve_gateway(
        &self,
        destination: IpAddress,
        table: u32,
        required_oif: Option<u32>,
        minimum_scope: Option<u8>,
    ) -> Option<u32> {
        let winner = self.lookup_key(FibLookupKey::gateway(
            destination,
            table,
            required_oif,
            minimum_scope,
        ))?;
        if winner.matched.gateway.is_some()
            || (is_ipv4(destination) && winner.matched.scope < RT_SCOPE_LINK)
        {
            return None;
        }
        Some(winner.oif)
    }

    pub(super) fn insert(
        &mut self,
        route: RouteEntry,
        flags: RouteNewFlags,
    ) -> Result<RouteMutationOutcome, SystemError> {
        // A conflict group is contiguous by the insertion invariant. Track
        // its bounds directly instead of allocating a temporary index vector
        // for every route request.
        let mut first = None;
        let mut last = None;
        let mut exact = None;
        for (index, existing) in self.entries.iter().copied().enumerate() {
            if conflict_group(existing, route) {
                first.get_or_insert(index);
                last = Some(index);
                if existing == route {
                    exact = Some(index);
                }
            }
        }

        if flags.excl && first.is_some() {
            return Err(SystemError::EEXIST);
        }
        if flags.replace {
            if let Some(first) = first {
                if is_ipv4(route.destination.address()) {
                    if exact == Some(first) {
                        return Ok(RouteMutationOutcome::Unchanged(route));
                    }
                    if exact.is_some() {
                        return Err(SystemError::EEXIST);
                    }
                }
                let old = core::mem::replace(&mut self.entries[first], route);
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
        if !is_ipv4(route.destination.address()) && first.is_some() {
            return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
        }

        let position = if let Some(last) = last {
            if flags.append {
                last + 1
            } else {
                first.expect("a last conflict implies a first conflict")
            }
        } else {
            self.entries
                .iter()
                .position(|existing| {
                    same_prefix_domain(*existing, route) && existing.priority > route.priority
                })
                .unwrap_or(self.entries.len())
        };
        self.entries
            .try_reserve(1)
            .map_err(|_| SystemError::ENOMEM)?;
        self.entries.insert(position, route);
        Ok(RouteMutationOutcome::Added {
            route,
            appended: flags.append && first.is_some(),
        })
    }

    pub(super) fn insert_derived(&mut self, route: RouteEntry) -> Result<bool, SystemError> {
        if self.entries.contains(&route) {
            return Ok(false);
        }
        let position = self
            .entries
            .iter()
            .position(|existing| {
                same_prefix_domain(*existing, route) && existing.priority > route.priority
            })
            .unwrap_or(self.entries.len());
        self.entries
            .try_reserve(1)
            .map_err(|_| SystemError::ENOMEM)?;
        self.entries.insert(position, route);
        Ok(true)
    }

    pub(super) fn delete(
        &mut self,
        selector: RouteDeleteSelector,
    ) -> Result<RouteEntry, SystemError> {
        let index = self
            .entries
            .iter()
            .position(|route| delete_matches(*route, selector))
            .ok_or(SystemError::ESRCH)?;
        Ok(self.entries.remove(index))
    }

    pub(super) fn delta_from(&self, before: &Self) -> Result<FibDelta, SystemError> {
        diff_routes(&before.entries, &self.entries)
    }

    pub(super) fn reconcile_address_routes(
        &mut self,
        before_routes: &[RouteEntry],
        after_routes: &[RouteEntry],
        ifindex: u32,
        deleted_address: Option<IpAddress>,
    ) -> Result<PreferredSourceTransitions, SystemError> {
        let mut changed_existing_prefixes = Vec::new();
        changed_existing_prefixes
            .try_reserve_exact(before_routes.len())
            .map_err(|_| SystemError::ENOMEM)?;
        for old in before_routes.iter().copied() {
            if after_routes.contains(&old) {
                continue;
            }
            if let Some(index) = self.entries.iter().position(|entry| *entry == old) {
                self.entries.remove(index);
                if after_routes.iter().any(|new| same_derived_slot(*new, old)) {
                    changed_existing_prefixes.push(old);
                }
            }
        }
        for new in after_routes.iter().copied() {
            let prefix_existed = before_routes.iter().any(|old| same_derived_slot(*old, new));
            if !self.entries.contains(&new)
                && (!prefix_existed
                    || changed_existing_prefixes
                        .iter()
                        .any(|old| same_derived_slot(*old, new)))
            {
                self.insert_derived(new)?;
            }
        }

        let mut transitions = PreferredSourceTransitions::default();
        if let Some(deleted) = deleted_address {
            let mut index = 0;
            while index < self.entries.len() {
                let entry = self.entries[index];
                if entry.preferred_source == Some(deleted)
                    && !(entry.oif == ifindex && after_routes.contains(&entry))
                {
                    if entry.oif != ifindex {
                        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
                    }
                    if is_ipv4(deleted) {
                        self.entries.remove(index);
                        continue;
                    }
                    let old = self.entries[index];
                    self.entries[index].preferred_source = None;
                    transitions
                        .removed
                        .try_reserve(1)
                        .map_err(|_| SystemError::ENOMEM)?;
                    transitions
                        .added
                        .try_reserve(1)
                        .map_err(|_| SystemError::ENOMEM)?;
                    transitions.removed.push(old);
                    transitions.added.push(self.entries[index]);
                }
                index += 1;
            }
        }
        Ok(transitions)
    }
}

#[derive(Default)]
pub(super) struct PreferredSourceTransitions {
    pub removed: Vec<RouteEntry>,
    pub added: Vec<RouteEntry>,
}

fn diff_routes(before: &[RouteEntry], after: &[RouteEntry]) -> Result<FibDelta, SystemError> {
    let mut before_index = Vec::new();
    before_index
        .try_reserve_exact(before.len())
        .map_err(|_| SystemError::ENOMEM)?;
    before_index.extend_from_slice(before);
    before_index.sort_unstable();
    let mut after_index = Vec::new();
    after_index
        .try_reserve_exact(after.len())
        .map_err(|_| SystemError::ENOMEM)?;
    after_index.extend_from_slice(after);
    after_index.sort_unstable();
    let mut removed = Vec::new();
    for entry in before.iter().copied() {
        if after_index.binary_search(&entry).is_err() {
            removed.try_reserve(1).map_err(|_| SystemError::ENOMEM)?;
            removed.push(entry);
        }
    }
    let mut added = Vec::new();
    for entry in after.iter().copied() {
        if before_index.binary_search(&entry).is_err() {
            added.try_reserve(1).map_err(|_| SystemError::ENOMEM)?;
            added.push(entry);
        }
    }
    Ok(FibDelta { removed, added })
}

fn same_derived_slot(left: RouteEntry, right: RouteEntry) -> bool {
    left.destination == right.destination
        && left.table == right.table
        && left.kind == right.kind
        && left.oif == right.oif
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

fn conflict_group(left: RouteEntry, right: RouteEntry) -> bool {
    same_prefix_domain(left, right)
        && left.priority == right.priority
        && (is_ipv4(left.destination.address()) && left.tos == right.tos
            || !is_ipv4(left.destination.address()))
}

fn same_prefix_domain(left: RouteEntry, right: RouteEntry) -> bool {
    left.table == right.table
        && super::canonical_cidr(left.destination) == super::canonical_cidr(right.destination)
        && same_family(left.destination.address(), right.destination.address())
}
