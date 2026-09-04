use alloc::vec::Vec;

use smoltcp::wire::IpAddress;
use system_error::SystemError;

use super::{
    builtin_rule_tables,
    fib_index::{projection_key, AliasPlacement, BroadcastLookup, FibIndex, ProjectionKey},
    is_ipv4, RouteDeleteSelector, RouteEntry, RouteLookupResult, RouteMutationOutcome,
    RouteNewFlags, RouteNotifications, RouteSourcePolicy, RT_SCOPE_LINK, RT_TABLE_DEFAULT,
    RT_TABLE_LOCAL,
};

#[derive(Debug, Default, PartialEq, Eq)]
pub(in crate::net) struct FibTable {
    entries: Vec<RouteEntry>,
    index: FibIndex,
}

pub(super) struct FibDelta {
    pub removed: Vec<RouteEntry>,
    pub added: Vec<RouteEntry>,
}

#[derive(Debug, Clone, Copy)]
pub(super) enum FibEdit {
    Insert {
        index: usize,
        route: RouteEntry,
        placement: AliasPlacement,
    },
    Replace {
        index: usize,
        old: RouteEntry,
        new: RouteEntry,
    },
    Delete {
        index: usize,
        route: RouteEntry,
    },
    None,
}

pub(super) struct PlannedFibMutation<T> {
    pub edit: FibEdit,
    pub outcome: T,
}

impl FibEdit {
    fn removes_index(self, index: usize) -> bool {
        matches!(
            self,
            Self::Replace {
                index: removed, ..
            } | Self::Delete {
                index: removed, ..
            } if removed == index
        )
    }

    fn inserted_route(self) -> Option<(usize, RouteEntry)> {
        match self {
            Self::Insert { index, route, .. } => Some((index, route)),
            Self::Replace { index, new, .. } => Some((index, new)),
            Self::Delete { .. } | Self::None => None,
        }
    }
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

    pub(super) fn insert_derived(&mut self, route: RouteEntry) -> Result<bool, SystemError> {
        let inserted = self.fib.insert_derived(route)?;
        if inserted {
            self.record(route.oif)?;
        }
        Ok(inserted)
    }

    pub(super) fn remove_where(
        &mut self,
        predicate: impl Fn(RouteEntry) -> bool,
    ) -> Result<Vec<RouteEntry>, SystemError> {
        // Record every affected projection before mutating the candidate. If
        // bookkeeping allocation fails, the transaction can still abort with
        // its candidate untouched.
        let removed_count = self
            .fib
            .entries
            .iter()
            .filter(|route| predicate(**route))
            .count();
        let mut removed = Vec::new();
        removed
            .try_reserve_exact(removed_count)
            .map_err(|_| SystemError::ENOMEM)?;
        let mut old_to_new = Vec::new();
        old_to_new
            .try_reserve_exact(self.fib.entries.len())
            .map_err(|_| SystemError::ENOMEM)?;
        let mut next_index = 0;
        for index in 0..self.fib.entries.len() {
            let route = self.fib.entries[index];
            if predicate(route) {
                self.record(route.oif)?;
                removed.push(route);
                old_to_new.push(None);
            } else {
                old_to_new.push(Some(next_index));
                next_index += 1;
            }
        }
        if removed.is_empty() {
            return Ok(removed);
        }
        let mut old_index = 0;
        self.fib.entries.retain(|_| {
            let retain = old_to_new[old_index].is_some();
            old_index += 1;
            retain
        });
        self.fib.index.commit_retain(&old_to_new, &self.fib.entries);
        Ok(removed)
    }

    pub(super) fn reconcile_address_routes<F>(
        &mut self,
        before_routes: &[RouteEntry],
        after_routes: &[RouteEntry],
        ifindex: u32,
        deleted_addresses: &[IpAddress],
        preferred_source_remains_usable: F,
    ) -> Result<PreferredSourceTransitions, SystemError>
    where
        F: Fn(RouteEntry) -> bool,
    {
        // Reserve mutation bookkeeping first. On failure, the candidate is
        // still untouched; subsequent edits keep the candidate's index in
        // lockstep with its authoritative route vector.
        self.record(ifindex)?;
        if !deleted_addresses.is_empty() {
            let mut affected_oifs = Vec::new();
            affected_oifs
                .try_reserve_exact(self.fib.entries.len())
                .map_err(|_| SystemError::ENOMEM)?;
            for entry in self.fib.entries.iter().copied() {
                if entry
                    .preferred_source
                    .is_some_and(|source| deleted_addresses.contains(&source))
                    && !preferred_source_remains_usable(entry)
                    && (!entry.preferred_source.is_some_and(is_ipv4)
                        || entry.table == super::RT_TABLE_MAIN)
                    && !affected_oifs.contains(&entry.oif)
                {
                    affected_oifs.push(entry.oif);
                }
            }
            for oif in affected_oifs {
                self.record(oif)?;
            }
        }
        self.fib.reconcile_address_routes(
            before_routes,
            after_routes,
            deleted_addresses,
            preferred_source_remains_usable,
        )
    }

    pub(super) fn finish(self) -> Result<Vec<u32>, SystemError> {
        Ok(self.affected_oifs)
    }
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

impl FibTable {
    pub(super) fn try_clone(&self) -> Result<Self, SystemError> {
        let mut entries = Vec::new();
        entries
            .try_reserve_exact(self.entries.len())
            .map_err(|_| SystemError::ENOMEM)?;
        entries.extend_from_slice(&self.entries);
        Ok(Self {
            entries,
            index: self.index.try_clone()?,
        })
    }

    pub(super) fn snapshot(&self) -> Result<Vec<RouteEntry>, SystemError> {
        Ok(self.try_clone()?.entries)
    }

    pub(super) fn projection_for_iface(
        &self,
        ifindex: u32,
    ) -> Result<Vec<smoltcp::iface::Route>, SystemError> {
        self.index.projection_for_iface(&self.entries, ifindex)
    }

    pub(super) fn plan_insert(
        &self,
        route: RouteEntry,
        flags: RouteNewFlags,
    ) -> Result<PlannedFibMutation<RouteMutationOutcome>, SystemError> {
        let analysis = self.index.analyze_insert(route);
        let first = analysis.first;
        let exact = analysis.exact;

        if flags.excl && first.is_some() {
            return Err(SystemError::EEXIST);
        }
        if flags.replace {
            if let Some(first) = first {
                if is_ipv4(route.destination.address()) {
                    if exact == Some(first) {
                        return Ok(PlannedFibMutation {
                            edit: FibEdit::None,
                            outcome: RouteMutationOutcome::Unchanged(route),
                        });
                    }
                    if exact.is_some() {
                        return Err(SystemError::EEXIST);
                    }
                }
                let old = self.entries[first];
                return Ok(PlannedFibMutation {
                    edit: FibEdit::Replace {
                        index: first,
                        old,
                        new: route,
                    },
                    outcome: RouteMutationOutcome::Replaced { old, new: route },
                });
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

        let placement = if flags.append {
            self.index.append_placement(route)
        } else {
            analysis.placement
        };
        Ok(PlannedFibMutation {
            edit: FibEdit::Insert {
                index: self.entries.len(),
                route,
                placement,
            },
            outcome: RouteMutationOutcome::Added {
                route,
                appended: flags.append && first.is_some(),
            },
        })
    }

    pub(super) fn plan_delete(
        &self,
        selector: RouteDeleteSelector,
    ) -> Result<PlannedFibMutation<RouteEntry>, SystemError> {
        let probe = RouteEntry {
            destination: selector.destination,
            table: selector.table,
            source: None,
            preferred_source: None,
            priority: 0,
            tos: 0,
            protocol: 0,
            scope: 0,
            kind: 0,
            oif: 0,
            gateway: None,
            nexthop_flags: 0,
        };
        let index = self
            .index
            .prefix_candidates(probe)
            .iter()
            .copied()
            .filter(|index| delete_matches(self.entries[*index], selector))
            .min_by_key(|index| self.index.order_key(*index))
            .ok_or(SystemError::ESRCH)?;
        let route = self.entries[index];
        Ok(PlannedFibMutation {
            edit: FibEdit::Delete { index, route },
            outcome: route,
        })
    }

    pub(super) fn reserve_edit(&mut self, edit: FibEdit) -> Result<(), SystemError> {
        match edit {
            FibEdit::Insert { route, .. } => {
                self.entries
                    .try_reserve(1)
                    .map_err(|_| SystemError::ENOMEM)?;
                self.index.prepare_insert(route)
            }
            FibEdit::Replace { new, .. } => self.index.prepare_insert(new),
            FibEdit::Delete { .. } | FibEdit::None => Ok(()),
        }
    }

    pub(super) fn cancel_edit_reservation(&mut self, edit: FibEdit) {
        match edit {
            FibEdit::Insert { route, .. } | FibEdit::Replace { new: route, .. } => {
                self.index.cancel_insert_reservation(route)
            }
            FibEdit::Delete { .. } | FibEdit::None => {}
        }
    }

    pub(super) fn apply_edit(&mut self, edit: FibEdit) {
        match edit {
            FibEdit::Insert {
                index,
                route,
                placement,
            } => {
                debug_assert_eq!(index, self.entries.len());
                self.entries.push(route);
                self.index
                    .commit_insert(index, route, placement, &self.entries);
            }
            FibEdit::Replace { index, old, new } => {
                self.entries[index] = new;
                self.index.commit_replace(index, old, new, &self.entries);
            }
            FibEdit::Delete { index, route } => {
                let removed = self.remove_at(index);
                debug_assert_eq!(removed, route);
            }
            FibEdit::None => {}
        }
    }

    fn remove_at(&mut self, index: usize) -> RouteEntry {
        let route = self.entries[index];
        let last_index = self.entries.len() - 1;
        let moved = (index != last_index).then(|| (last_index, self.entries[last_index]));
        let removed = self.entries.swap_remove(index);
        self.index.commit_remove(index, route, moved, &self.entries);
        removed
    }

    pub(super) fn projection_keys(edit: FibEdit) -> [Option<ProjectionKey>; 2] {
        let mut keys = [None, None];
        let mut add_key = |key: Option<ProjectionKey>| {
            if let Some(key) = key {
                if keys[0] != Some(key) {
                    if keys[0].is_none() {
                        keys[0] = Some(key);
                    } else {
                        keys[1] = Some(key);
                    }
                }
            }
        };
        match edit {
            FibEdit::Insert { route, .. } => add_key(projection_key(route)),
            FibEdit::Replace { old, new, .. } => {
                add_key(projection_key(old));
                add_key(projection_key(new));
            }
            FibEdit::Delete { route, .. } => add_key(projection_key(route)),
            FibEdit::None => {}
        }
        keys
    }

    pub(super) fn projection_winner(
        &self,
        key: ProjectionKey,
        edit: Option<FibEdit>,
    ) -> Option<RouteEntry> {
        let Some(edit) = edit else {
            return self
                .index
                .projection_winner(key)
                .map(|index| self.entries[index]);
        };
        if let FibEdit::Insert {
            route, placement, ..
        } = edit
        {
            let current = self.index.projection_winner(key);
            if projection_key(route) != Some(key) {
                return current.map(|index| self.entries[index]);
            }
            return current
                .filter(|index| {
                    !self.index.projected_insert_precedes(
                        route,
                        placement,
                        *index,
                        self.entries[*index],
                    )
                })
                .map(|index| self.entries[index])
                .or(Some(route));
        }

        let mut winner = self
            .index
            .projection_candidates(key)
            .iter()
            .copied()
            .filter(|index| !edit.removes_index(*index))
            .min_by_key(|index| {
                (
                    u8::from(self.entries[*index].table != RT_TABLE_LOCAL),
                    self.index.order_key(*index),
                )
            });
        if let Some((index, route)) = edit
            .inserted_route()
            .filter(|(_, route)| projection_key(*route) == Some(key))
        {
            let inserted_order = (
                u8::from(route.table != RT_TABLE_LOCAL),
                self.index.order_key(index),
            );
            if winner.is_none_or(|current| {
                inserted_order
                    < (
                        u8::from(self.entries[current].table != RT_TABLE_LOCAL),
                        self.index.order_key(current),
                    )
            }) {
                winner = Some(index);
            }
            return winner.map(|current| {
                if current == index {
                    route
                } else {
                    self.entries[current]
                }
            });
        }
        winner.map(|index| self.entries[index])
    }

    fn lookup_key(&self, key: FibLookupKey) -> Option<RouteLookupResult> {
        let route_index = self.index.lookup(
            &self.entries,
            key.destination,
            key.table,
            key.required_oif,
            key.minimum_scope,
            key.broadcast,
        )?;
        let route = self.entries[route_index];
        Some(RouteLookupResult {
            oif: route.oif,
            next_hop: route.gateway.unwrap_or(key.destination),
            source: route
                .preferred_source
                .map(RouteSourcePolicy::Preferred)
                .unwrap_or(RouteSourcePolicy::SelectConfigured),
            table: route.table,
            matched: route,
        })
    }

    fn lookup_builtin_rules(
        &self,
        destination: IpAddress,
        mut key: impl FnMut(u32) -> FibLookupKey,
    ) -> Option<RouteLookupResult> {
        builtin_rule_tables(destination)
            .iter()
            .find_map(|table| self.lookup_key(key(*table)))
    }

    pub(super) fn lookup_output(&self, destination: IpAddress) -> Option<RouteLookupResult> {
        self.lookup_builtin_rules(destination, |table| {
            FibLookupKey::output(destination, table)
        })
    }

    pub(super) fn lookup_on_iface(
        &self,
        destination: IpAddress,
        oif: u32,
    ) -> Option<RouteLookupResult> {
        self.lookup_builtin_rules(destination, |table| {
            FibLookupKey::on_iface(destination, table, oif)
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
        self.lookup_builtin_rules(destination, |table| {
            if table == RT_TABLE_LOCAL {
                FibLookupKey::ingress_local(destination, ingress_oif)
            } else {
                FibLookupKey::output(destination, table)
            }
        })
    }

    /// A flat smoltcp LPM projection cannot preserve RPDB table priority when
    /// a more-specific default-table route overlaps a main-table route. Keep
    /// such namespaces on the authoritative IPv4 egress path instead.
    pub(in crate::net) fn requires_authoritative_ipv4_output(&self) -> bool {
        self.index.has_ipv4_routes(RT_TABLE_DEFAULT)
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

    /// Resolve a gateway using Linux's table-validation order: a non-main
    /// route first consults its own table, then falls back to the built-in
    /// family rule chain. The chain is shared with packet lookup so IPv4's
    /// default-table fallback and IPv6's lack of one cannot diverge.
    pub(super) fn resolve_gateway_with_builtin_rules(
        &self,
        destination: IpAddress,
        route_table: u32,
        required_oif: Option<u32>,
        minimum_scope: Option<u8>,
    ) -> Option<u32> {
        let in_route_table = (route_table != super::RT_TABLE_MAIN)
            .then(|| self.resolve_gateway(destination, route_table, required_oif, minimum_scope))
            .flatten();
        in_route_table.or_else(|| {
            builtin_rule_tables(destination)
                .iter()
                .copied()
                .filter(|table| route_table == super::RT_TABLE_MAIN || *table != route_table)
                .find_map(|table| {
                    self.resolve_gateway(destination, table, required_oif, minimum_scope)
                })
        })
    }

    fn insert_derived(&mut self, route: RouteEntry) -> Result<bool, SystemError> {
        let analysis = self.index.analyze_insert(route);
        if analysis.exact.is_some() {
            return Ok(false);
        }
        // Linux installs address-derived routes with NLM_F_APPEND: an
        // existing equal-metric alias keeps precedence over the new kernel
        // route.
        let placement = self.index.append_placement(route);
        let index = self.entries.len();
        self.reserve_edit(FibEdit::Insert {
            index,
            route,
            placement,
        })?;
        self.apply_edit(FibEdit::Insert {
            index,
            route,
            placement,
        });
        Ok(true)
    }

    pub(super) fn delta_from(&self, before: &Self) -> Result<FibDelta, SystemError> {
        diff_routes(&before.entries, &self.entries)
    }

    fn reconcile_address_routes<F>(
        &mut self,
        before_routes: &[RouteEntry],
        after_routes: &[RouteEntry],
        deleted_addresses: &[IpAddress],
        preferred_source_remains_usable: F,
    ) -> Result<PreferredSourceTransitions, SystemError>
    where
        F: Fn(RouteEntry) -> bool,
    {
        let mut changed_existing_prefixes = Vec::new();
        changed_existing_prefixes
            .try_reserve_exact(before_routes.len())
            .map_err(|_| SystemError::ENOMEM)?;
        for old in before_routes.iter().copied() {
            if after_routes.contains(&old) {
                continue;
            }
            if let Some(index) = self.entries.iter().position(|entry| *entry == old) {
                self.remove_at(index);
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
        if !deleted_addresses.is_empty() {
            let mut index = 0;
            while index < self.entries.len() {
                let entry = self.entries[index];
                let Some(deleted) = entry
                    .preferred_source
                    .filter(|source| deleted_addresses.contains(source))
                else {
                    index += 1;
                    continue;
                };
                if !preferred_source_remains_usable(entry) {
                    if is_ipv4(deleted) {
                        // fib_sync_down_addr() only invalidates the L3
                        // domain's forwarding table. DragonOS has no VRF/L3
                        // master yet, so that table is RT_TABLE_MAIN.
                        if entry.table != super::RT_TABLE_MAIN {
                            index += 1;
                            continue;
                        }
                        self.remove_at(index);
                        continue;
                    }
                    // Linux IPv6 keeps the route and silently drops a stale
                    // preferred source. DragonOS additionally requires IPv6
                    // preferred sources to stay on the route's egress iface
                    // until namespace-level IPv6 local delivery exists.
                    let old = self.entries[index];
                    let new = RouteEntry {
                        preferred_source: None,
                        ..old
                    };
                    self.entries[index] = new;
                    self.index.commit_replace(index, old, new, &self.entries);
                    transitions
                        .removed
                        .try_reserve(1)
                        .map_err(|_| SystemError::ENOMEM)?;
                    transitions
                        .added
                        .try_reserve(1)
                        .map_err(|_| SystemError::ENOMEM)?;
                    transitions.removed.push(old);
                    transitions.added.push(new);
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
