use alloc::vec::Vec;

use smoltcp::iface::Route as SmolRoute;
use smoltcp::wire::{IpAddress, IpCidr, Ipv4Address, Ipv6Address};
use system_error::SystemError;

use super::{
    builtin_rule_tables,
    fib_index::{
        learned_projection, projection_key, AliasPlacement, BroadcastLookup, FibIndex,
        ProjectionKey,
    },
    is_ipv4, RouteDeleteSelector, RouteEntry, RouteLookupResult, RouteMutationOutcome,
    RouteNewFlags, RouteNotifications, RouteSourcePolicy, RT_SCOPE_LINK, RT_TABLE_DEFAULT,
    RT_TABLE_LOCAL, RT_TABLE_MAIN,
};

// Router advertisements must not grow the namespace FIB without bound.
// All learned defaults belong to the main table's IPv6 default prefix.
const MAX_LEARNED_DEFAULT_ROUTES: usize = 64;
const MAX_LEARNED_PREFIX_ROUTES: usize = 256;

#[derive(Debug, Default, PartialEq, Eq)]
pub(in crate::net) struct FibTable {
    entries: Vec<RouteEntry>,
    index: FibIndex,
}

impl FibTable {
    pub(super) fn dump_entry(&self, index: usize) -> Option<RouteEntry> {
        self.entries.get(index).copied()
    }

    /// Creation rollback may skip route preparation only for devices that
    /// have never acquired an authoritative FIB entry.
    pub(in crate::net) fn has_oif(&self, ifindex: u32) -> bool {
        self.entries.iter().any(|route| route.oif == ifindex)
    }

    /// A netns is already dying, so no caller can abort its peer deletion.
    /// Remove only this device's routes in place: deleting index entries does
    /// not allocate, unlike building an ordinary rollback-capable snapshot.
    pub(in crate::net) fn purge_oif_for_netns_teardown(&mut self, ifindex: u32) {
        while let Some(index) = self.entries.iter().position(|route| route.oif == ifindex) {
            self.remove_at(index);
        }
    }
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

    pub(super) fn update_address_prefix(
        &mut self,
        route: RouteEntry,
        replace_permanent: bool,
    ) -> Result<Option<(RouteEntry, RouteEntry)>, SystemError> {
        if let Some(index) = self.fib.address_prefix_slot(route) {
            let old = self.fib.entries[index];
            if replace_permanent || old.expires_at_us().is_some() {
                return self.replace_address_prefix(index, old, route);
            }
            return Ok(None);
        }
        if route.expires_at_us().is_some() {
            let occupied = self
                .fib
                .index
                .prefix_candidates(route)
                .iter()
                .any(|&index| {
                    let current = self.fib.entries[index];
                    !current.is_ra_managed() && same_on_link_alias(current, route)
                });
            if occupied {
                return Ok(None);
            }
            if let Some(index) = self.fib.prefix_route_slot(route) {
                let old = self.fib.entries[index];
                if old.expires_at_us().is_some() {
                    return self.replace_address_prefix(index, old, route);
                }
            }
        }
        self.insert_derived(route)?;
        Ok(None)
    }

    pub(super) fn remove_permanent_address_prefix(
        &mut self,
        route: RouteEntry,
        remaining_deadline: Option<i64>,
    ) -> Result<Option<(RouteEntry, RouteEntry)>, SystemError> {
        let Some(index) = self.fib.address_prefix_slot(route) else {
            return Ok(None);
        };
        let old = self.fib.entries[index];
        if let Some(deadline) = remaining_deadline {
            if old.expires_at_us().is_none() {
                return self.replace_address_prefix(
                    index,
                    old,
                    old.with_address_prefix_deadline(Some(deadline)),
                );
            }
        } else {
            self.record(old.oif)?;
            self.fib.apply_edit(FibEdit::Delete { index, route: old });
        }
        Ok(None)
    }

    fn replace_address_prefix(
        &mut self,
        index: usize,
        old: RouteEntry,
        new: RouteEntry,
    ) -> Result<Option<(RouteEntry, RouteEntry)>, SystemError> {
        if old == new {
            return Ok(None);
        }
        self.record(old.oif)?;
        let edit = FibEdit::Replace { index, old, new };
        self.fib.reserve_edit(edit)?;
        self.fib.apply_edit(edit);
        Ok(Some((old, new)))
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
pub(super) struct FibLookupKey {
    pub(super) destination: IpAddress,
    pub(super) source: Option<Ipv6Address>,
    pub(super) table: u32,
    pub(super) required_oif: Option<u32>,
    pub(super) preferred_oif: Option<u32>,
    pub(super) minimum_scope: Option<u8>,
    pub(super) broadcast: BroadcastLookup,
}

impl FibLookupKey {
    fn output(destination: IpAddress, table: u32) -> Self {
        Self {
            destination,
            source: None,
            table,
            required_oif: None,
            preferred_oif: None,
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

    fn gateway(
        destination: IpAddress,
        table: u32,
        required_oif: Option<u32>,
        minimum_scope: Option<u8>,
    ) -> Self {
        Self {
            destination,
            source: None,
            table,
            required_oif,
            preferred_oif: None,
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

    pub(super) fn snapshot_ipv4_main(&self) -> Result<Vec<super::Ipv4MainRoute>, SystemError> {
        let visible = self.entries.iter().enumerate().filter(|(_, entry)| {
            entry.table == RT_TABLE_MAIN
                && is_ipv4(entry.destination.address())
                && !matches!(entry.kind, super::RTN_BROADCAST | super::RTN_MULTICAST)
        });
        let mut entries = Vec::new();
        entries
            .try_reserve_exact(visible.clone().count())
            .map_err(|_| SystemError::ENOMEM)?;
        entries.extend(visible.map(|(index, entry)| super::Ipv4MainRoute {
            entry: *entry,
            order: self.index.order_key(index),
        }));
        Ok(entries)
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
        let pio_slot = self.prefix_route_slot(route);
        let first = pio_slot.or(analysis.first);
        let exact = analysis.exact.or_else(|| self.address_prefix_alias(route));
        let matching_metric = || {
            self.index
                .prefix_candidates(route)
                .iter()
                .copied()
                .filter(|&index| {
                    let candidate = self.entries[index];
                    candidate.source == route.source && candidate.priority == route.priority
                })
        };

        if flags.excl && first.is_some() {
            return Err(SystemError::EEXIST);
        }
        if flags.replace {
            if let Some(mut first) = first {
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
                } else if pio_slot.is_none()
                    && route.destination.prefix_len() == 0
                    && matching_metric()
                        .any(|index| self.entries[index].learned_default().is_some())
                {
                    // Linux finds a route with the same nexthop class before
                    // falling back to the first alias. A learned default is
                    // not an ECMP-eligible gateway route.
                    first = matching_metric()
                        .filter(|&index| {
                            let current = self.entries[index];
                            let gateway_route =
                                current.learned_default().is_none() && current.gateway.is_some();
                            gateway_route == route.gateway.is_some()
                        })
                        .min_by_key(|&index| self.index.order_key(index))
                        .unwrap_or(first);
                    if exact.is_some_and(|slot| slot != first) {
                        return Err(SystemError::EEXIST);
                    }
                    if self.entries[first].learned_default().is_some()
                        && matching_metric()
                            .any(|index| self.entries[index].learned_default().is_none())
                    {
                        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
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
        } else if exact.is_some() || pio_slot.is_some() {
            return Err(SystemError::EEXIST);
        }

        if !flags.create {
            return Err(SystemError::ENOENT);
        }
        if !is_ipv4(route.destination.address()) && first.is_some() {
            if !matching_metric().any(|index| self.entries[index].learned_default().is_some()) {
                return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
            }
            if !flags.append
                || matching_metric().any(|index| {
                    let current = self.entries[index];
                    current.oif == route.oif && current.gateway == route.gateway
                })
            {
                return Err(SystemError::EEXIST);
            }
            // RA aliases may coexist with one configured route. Admitting a
            // second would expose unsupported IPv6 multipath after RA expiry.
            if matching_metric().any(|index| self.entries[index].learned_default().is_none()) {
                return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
            }
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

    /// A learned default is owned by the advertising gateway and interface.
    /// An administrator's equal-metric route to that same next hop wins the
    /// slot even if the administrator chose the same protocol number.
    pub(super) fn plan_learned_default(&self, route: RouteEntry, withdraw: bool) -> FibEdit {
        debug_assert!(route.learned_default().is_some());
        let mut learned = None;
        let mut learned_count = 0;
        for &index in self.index.prefix_candidates(route) {
            let current = self.entries[index];
            let is_learned = current.learned_default().is_some();
            learned_count += usize::from(is_learned);
            if current.oif != route.oif
                || current.gateway != route.gateway
                || current.priority != route.priority
            {
                continue;
            }
            if is_learned {
                learned = Some((index, current));
            } else if !withdraw && current.kind == route.kind && current.source.is_none() {
                return FibEdit::None;
            }
        }
        match (learned, withdraw) {
            (Some((index, old)), false) if old != route => FibEdit::Replace {
                index,
                old,
                new: route,
            },
            (Some((index, route)), true) => FibEdit::Delete { index, route },
            (None, false) if learned_count < MAX_LEARNED_DEFAULT_ROUTES => FibEdit::Insert {
                index: self.entries.len(),
                route,
                placement: self.index.append_placement(route),
            },
            _ => FibEdit::None,
        }
    }

    pub(super) fn learned_defaults(&self) -> impl Iterator<Item = RouteEntry> + '_ {
        let default = IpCidr::new(smoltcp::wire::Ipv6Address::UNSPECIFIED.into(), 0);
        self.index
            .prefix_candidates_for(RT_TABLE_MAIN, default)
            .iter()
            .map(|&index| self.entries[index])
            .filter(|route| route.learned_default().is_some())
    }

    pub(super) fn expiring_ipv6_routes(&self) -> impl Iterator<Item = RouteEntry> + '_ {
        self.entries
            .iter()
            .copied()
            .filter(|route| route.expires_at_us().is_some())
    }

    pub(super) fn plan_expired_route(&self, route: RouteEntry) -> FibEdit {
        self.index
            .prefix_candidates(route)
            .iter()
            .copied()
            .find(|&index| self.entries[index] == route)
            .map_or(FibEdit::None, |index| FibEdit::Delete { index, route })
    }

    fn address_prefix_slot(&self, route: RouteEntry) -> Option<usize> {
        self.index
            .prefix_candidates(route)
            .iter()
            .copied()
            .find(|&index| {
                let current = self.entries[index];
                current.is_address_prefix() && same_on_link_alias(current, route)
            })
    }

    fn prefix_route_slot(&self, route: RouteEntry) -> Option<usize> {
        self.index
            .prefix_candidates(route)
            .iter()
            .copied()
            .find(|&index| {
                let current = self.entries[index];
                current.is_ra_prefix() && same_on_link_alias(current, route)
            })
    }

    pub(super) fn learned_prefix_slots(&self) -> usize {
        MAX_LEARNED_PREFIX_ROUTES.saturating_sub(
            self.entries
                .iter()
                .filter(|route| route.is_ra_prefix())
                .count(),
        )
    }

    pub(super) fn plan_learned_prefix(
        &self,
        route: RouteEntry,
        withdraw: bool,
        may_insert: bool,
    ) -> FibEdit {
        debug_assert!(route.is_ra_prefix());
        let mut existing = None;
        for &index in self.index.prefix_candidates(route) {
            let current = self.entries[index];
            if !same_on_link_alias(current, route) {
                continue;
            }
            if current.is_ra_prefix() {
                existing = Some((index, current));
            } else if !withdraw && !current.is_ra_managed() {
                // A nonzero PIO can refresh the expiration of an existing
                // finite on-link address route. Withdrawal still only removes
                // RA-owned routes.
                if current.is_address_prefix() && current.expires_at_us().is_some() {
                    let new = current.with_address_prefix_deadline(route.expires_at_us());
                    if new != current {
                        return FibEdit::Replace {
                            index,
                            old: current,
                            new,
                        };
                    }
                }
                return FibEdit::None;
            }
        }
        match (existing, withdraw) {
            (Some((index, old)), false) if old != route => FibEdit::Replace {
                index,
                old,
                new: route,
            },
            (Some((index, route)), true) => FibEdit::Delete { index, route },
            (None, false) if may_insert => FibEdit::Insert {
                index: self.entries.len(),
                route,
                placement: self.index.append_placement(route),
            },
            _ => FibEdit::None,
        }
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
            ipv6_address_local: false,
            ipv6_metadata: None,
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

    pub(super) fn learned_projection_after(
        &self,
        key: ProjectionKey,
        edit: FibEdit,
    ) -> Result<Option<Vec<SmolRoute>>, SystemError> {
        if !key.is_ipv6() {
            return Ok(None);
        }
        let existing = self
            .index
            .projection_candidates(key)
            .iter()
            .copied()
            .filter(|&index| !edit.removes_index(index))
            .map(|index| (self.entries[index], self.index.order_key(index)));
        let new = edit
            .inserted_route()
            .filter(|(_, route)| projection_key(*route) == Some(key))
            .map(|(index, route)| {
                let order = match edit {
                    FibEdit::Insert { placement, .. } => self.index.planned_order(route, placement),
                    _ => self.index.order_key(index),
                };
                (route, order)
            });
        learned_projection(existing.chain(new))
    }

    fn lookup_key(&self, key: FibLookupKey) -> Option<RouteLookupResult> {
        let route_index = self.index.lookup_key(&self.entries, key)?;
        Some(self.route_result(route_index, key.destination))
    }

    fn route_result(&self, route_index: usize, destination: IpAddress) -> RouteLookupResult {
        let route = self.entries[route_index];
        RouteLookupResult {
            oif: route.oif,
            next_hop: route.gateway.unwrap_or(destination),
            source: route
                .preferred_source
                .map(RouteSourcePolicy::Preferred)
                .unwrap_or(RouteSourcePolicy::SelectConfigured),
            table: route.table,
            matched: route,
        }
    }

    fn lookup_builtin_rules(
        &self,
        destination: IpAddress,
        mut key: impl FnMut(u32) -> FibLookupKey,
    ) -> Option<RouteLookupResult> {
        builtin_rule_tables(destination).iter().find_map(|table| {
            self.lookup_key(key(*table))
                .filter(|route| route.matched.kind != super::RTN_THROW)
        })
    }

    pub(super) fn lookup_output(&self, destination: IpAddress) -> Option<RouteLookupResult> {
        self.lookup_builtin_rules(destination, |table| {
            FibLookupKey::output(destination, table)
        })
    }

    pub(super) fn lookup_ipv6_output(
        &self,
        destination: Ipv6Address,
        oif: Option<u32>,
        source: Option<Ipv6Address>,
    ) -> Option<RouteLookupResult> {
        let destination = IpAddress::Ipv6(destination);
        self.lookup_builtin_rules(destination, |table| FibLookupKey {
            source,
            ..match oif {
                Some(oif) => FibLookupKey::on_iface(destination, table, oif),
                None => FibLookupKey::output(destination, table),
            }
        })
    }

    /// The IPv6 output RPDB uses built-in tables, whose IDs fit in one byte.
    /// The low bytes identify the original table and destination prefix.
    pub(super) fn ipv6_route_cookie(
        &self,
        route: RouteLookupResult,
        destination: Ipv6Address,
        required_oif: Option<u32>,
    ) -> u64 {
        let Ok(table) = u8::try_from(route.table) else {
            return 0;
        };
        let serial = self.index.ipv6_serial(route.matched);
        let length = route.matched.destination.prefix_len();
        let cookie = (serial << 16) | (u64::from(table) << 8) | u64::from(length);
        if self
            .cached_ipv6_output(destination, required_oif, cookie)
            .is_some_and(|cached| cached.matched == route.matched)
        {
            cookie
        } else {
            0
        }
    }

    fn cached_ipv6_output(
        &self,
        destination: Ipv6Address,
        required_oif: Option<u32>,
        cookie: u64,
    ) -> Option<RouteLookupResult> {
        let destination = IpAddress::Ipv6(destination);
        let table = (cookie >> 8) as u8 as u32;
        let prefix_len = cookie as u8;
        let index = self.index.ipv6_cookie_winner(
            &self.entries,
            destination,
            table,
            prefix_len,
            cookie >> 16,
            required_oif,
        )?;
        Some(self.route_result(index, destination))
    }

    pub(super) fn lookup_ipv6_socket_output(
        &self,
        destination: Ipv6Address,
        required_oif: Option<u32>,
        source: Option<Ipv6Address>,
        cookie: u64,
    ) -> Option<RouteLookupResult> {
        if cookie != 0 && self.has_ipv6_source_routes() {
            if let Some(route) = self.cached_ipv6_output(destination, required_oif, cookie) {
                return Some(route);
            }
        }
        self.lookup_ipv6_output(destination, required_oif, source)
    }

    /// Linux inet_addr_type() queries only the IPv4 local table, independent
    /// of an output route through main/default. The special address classes
    /// are determined before looking up that table.
    pub(super) fn ipv4_addr_type(&self, address: Ipv4Address) -> u8 {
        let octets = address.octets();
        if octets == [0, 0, 0, 0] || octets == [255, 255, 255, 255] {
            return super::RTN_BROADCAST;
        }
        if address.is_multicast() {
            return super::RTN_MULTICAST;
        }
        self.lookup_key(FibLookupKey::output(
            IpAddress::Ipv4(address),
            RT_TABLE_LOCAL,
        ))
        .map_or(super::RTN_UNICAST, |route| route.matched.kind)
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
        source: Option<Ipv6Address>,
    ) -> Option<RouteLookupResult> {
        // Multicast, link-local unicast and loopback require the input device.
        // Other IPv6 destinations prefer it among routes with the same rank.
        let required_oif = matches!(
            destination,
            IpAddress::Ipv6(address) if address.is_multicast() || address.is_unicast_link_local() || address.is_loopback()
        )
        .then_some(ingress_oif);
        let preferred_oif = (matches!(destination, IpAddress::Ipv6(_)) && required_oif.is_none())
            .then_some(ingress_oif);
        self.lookup_builtin_rules(destination, |table| FibLookupKey {
            source,
            required_oif,
            preferred_oif,
            ..FibLookupKey::output(destination, table)
        })
    }

    /// A flat smoltcp LPM projection cannot preserve RPDB table priority when
    /// a more-specific default-table route overlaps a main-table route. Keep
    /// such namespaces on the authoritative egress path instead. IPv6 also
    /// needs that path when main-table routing is configured: a socketless
    /// ICMPv6 reply may leave through a different interface from its owner.
    pub(in crate::net) fn requires_authoritative_output(&self) -> bool {
        self.index.has_ipv4_routes(RT_TABLE_DEFAULT)
            || self.index.has_ipv6_routes(RT_TABLE_MAIN)
            || self.has_ipv6_source_routes()
            || self.has_ipv6_special_routes()
    }

    pub(super) fn has_ipv6_special_routes(&self) -> bool {
        self.index.has_ipv6_special_routes()
    }

    pub(super) fn has_ipv6_source_routes(&self) -> bool {
        self.index.has_source_routes()
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
        Self::gateway_device(winner, destination)
    }

    fn gateway_device(winner: RouteLookupResult, destination: IpAddress) -> Option<u32> {
        if winner.matched.gateway.is_some()
            || super::is_ipv6_reject_or_throw(winner.matched.kind)
            || (is_ipv4(destination) && winner.matched.scope < RT_SCOPE_LINK)
        {
            return None;
        }
        Some(winner.oif)
    }

    /// Resolve a gateway using the built-in table order after checking the
    /// route's own table. IPv4 only needs the precheck for non-main routes.
    /// IPv6 validates the first raw winner of the fallback.
    pub(super) fn resolve_gateway_with_builtin_rules(
        &self,
        destination: IpAddress,
        route_table: u32,
        required_oif: Option<u32>,
        minimum_scope: Option<u8>,
    ) -> Option<u32> {
        if !is_ipv4(destination) {
            return self
                .resolve_gateway(destination, route_table, required_oif, minimum_scope)
                .or_else(|| {
                    // Linux nexthop validation examines the first raw FIB
                    // winner. A throw can skip a table for normal traffic,
                    // but is not a usable route to the gateway.
                    let winner = builtin_rule_tables(destination).iter().find_map(|&table| {
                        self.lookup_key(FibLookupKey::gateway(
                            destination,
                            table,
                            required_oif,
                            minimum_scope,
                        ))
                    })?;
                    Self::gateway_device(winner, destination)
                });
        }
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
        if analysis.exact.is_some() || self.address_prefix_alias(route).is_some() {
            return Ok(false);
        }
        if let Some(index) = self.prefix_route_slot(route) {
            let old = self.entries[index];
            // Linux admits a permanent connected route over an expiring
            // PIO alias. An already permanent PIO occupies the slot.
            if old.expires_at_us().is_none() {
                return Ok(false);
            }
            let edit = FibEdit::Replace {
                index,
                old,
                new: route,
            };
            self.reserve_edit(edit)?;
            self.apply_edit(edit);
            return Ok(true);
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

    /// An administrator can submit the same public route as an address
    /// prefix. Its internal ownership and deadline do not create a new alias.
    fn address_prefix_alias(&self, route: RouteEntry) -> Option<usize> {
        if is_ipv4(route.destination.address()) {
            return None;
        }
        self.index
            .prefix_candidates(route)
            .iter()
            .copied()
            .find(|&index| {
                let current = self.entries[index];
                let differs_by_origin = (current.is_address_prefix()
                    && route.ipv6_metadata.is_none())
                    || (route.is_address_prefix() && current.ipv6_metadata.is_none());
                differs_by_origin
                    && RouteEntry {
                        ipv6_metadata: route.ipv6_metadata,
                        ..current
                    } == route
            })
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
                    // preferred source.
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

fn same_on_link_alias(left: RouteEntry, right: RouteEntry) -> bool {
    same_derived_slot(left, right)
        && left.priority == right.priority
        && left.gateway.is_none()
        && right.gateway.is_none()
        && left.source.is_none()
        && right.source.is_none()
}

fn delete_matches(route: RouteEntry, selector: RouteDeleteSelector) -> bool {
    route.destination == selector.destination
        && (is_ipv4(route.destination.address()) || route.source == selector.source)
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

#[cfg(test)]
mod tests {
    use super::{FibEdit, FibTable, RouteEntry, MAX_LEARNED_DEFAULT_ROUTES};
    use crate::net::route::{LearnedDefaultRoute, RouterPreference, RTN_UNICAST, RT_TABLE_MAIN};
    use smoltcp::wire::{IpCidr, Ipv6Address};

    #[test]
    fn learned_default_capacity_preserves_refresh_and_withdrawal() {
        let route = |id: u16| RouteEntry {
            destination: IpCidr::new(Ipv6Address::UNSPECIFIED.into(), 0),
            source: None,
            preferred_source: None,
            table: RT_TABLE_MAIN,
            priority: 1024,
            tos: 0,
            protocol: 9, // RTPROT_RA
            scope: 0,
            kind: RTN_UNICAST,
            oif: u32::from(id % 2) + 1,
            gateway: Some(Ipv6Address::new(0xfe80, 0, 0, 0, 0, 0, 0, id).into()),
            nexthop_flags: 0,
            ipv6_address_local: false,
            ipv6_metadata: Some(crate::net::route::Ipv6RouteMetadata::RaDefault(
                LearnedDefaultRoute {
                    expires_at_us: 30_000_000,
                    preference: RouterPreference::Medium,
                },
            )),
        };
        let mut fib = FibTable::default();
        for id in 1..=MAX_LEARNED_DEFAULT_ROUTES {
            let edit = fib.plan_learned_default(route(id as u16), false);
            assert!(matches!(edit, FibEdit::Insert { .. }));
            fib.reserve_edit(edit).unwrap();
            fib.apply_edit(edit);
        }
        let new_router = route(MAX_LEARNED_DEFAULT_ROUTES as u16 + 1);
        assert!(matches!(
            fib.plan_learned_default(new_router, false),
            FibEdit::None
        ));

        let mut refreshed = route(1);
        let mut default = refreshed.learned_default().unwrap();
        default.expires_at_us += 1_000_000;
        refreshed.ipv6_metadata = Some(crate::net::route::Ipv6RouteMetadata::RaDefault(default));
        let edit = fib.plan_learned_default(refreshed, false);
        assert!(matches!(edit, FibEdit::Replace { .. }));
        fib.reserve_edit(edit).unwrap();
        fib.apply_edit(edit);
        assert_eq!(fib.learned_defaults().count(), MAX_LEARNED_DEFAULT_ROUTES);
        assert!(fib.learned_defaults().any(|entry| entry == refreshed));

        let edit = fib.plan_learned_default(refreshed, true);
        assert!(matches!(edit, FibEdit::Delete { .. }));
        fib.apply_edit(edit);
        assert!(matches!(
            fib.plan_learned_default(new_router, false),
            FibEdit::Insert { .. }
        ));
    }
}
