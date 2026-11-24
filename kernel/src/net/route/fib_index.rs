use alloc::{boxed::Box, vec::Vec};

use hashbrown::HashMap;
use smoltcp::iface::Route as SmolRoute;
use smoltcp::wire::{IpAddress, IpCidr, Ipv6Address};
use system_error::SystemError;

use super::{
    canonical_cidr, fib::FibLookupKey, RouteEntry, RouterPreference, RTN_BROADCAST, RTN_LOCAL,
    RTN_MULTICAST, RTN_UNICAST, RT_SCOPE_HOST, RT_SCOPE_LINK, RT_TABLE_LOCAL,
};

mod alias;

use alias::AliasIndex;
pub(super) use alias::{AliasInsertAnalysis, AliasOrder, AliasPlacement};

const RT_SCOPE_NOWHERE: u8 = 255;

#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq)]
enum AddressFamily {
    Ipv4,
    Ipv6,
}

impl AddressFamily {
    fn of(address: IpAddress) -> Self {
        match address {
            IpAddress::Ipv4(_) => Self::Ipv4,
            IpAddress::Ipv6(_) => Self::Ipv6,
        }
    }
}

#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq)]
struct PrefixKey {
    table: u32,
    family: AddressFamily,
    destination: IpAddress,
    prefix_len: u8,
}

#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq)]
struct PrefixDomain {
    table: u32,
    family: AddressFamily,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PrefixLengthCount {
    prefix_len: u8,
    count: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BroadcastLookup {
    Exclude,
    Any,
    OnIface(u32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScopeRequirement {
    Any,
    Link,
    Host,
    Nowhere,
}

impl ScopeRequirement {
    const ALL: [Self; 4] = [Self::Any, Self::Link, Self::Host, Self::Nowhere];

    fn from_minimum(minimum: Option<u8>) -> Option<Self> {
        match minimum {
            None => Some(Self::Any),
            Some(RT_SCOPE_LINK) => Some(Self::Link),
            Some(RT_SCOPE_HOST) => Some(Self::Host),
            Some(RT_SCOPE_NOWHERE) => Some(Self::Nowhere),
            // Gateway lookups derive their minimum as
            // max(RT_SCOPE_LINK, route_scope + 1), so no other value can
            // reach this private lookup surface.
            Some(_) => None,
        }
    }

    fn index(self) -> usize {
        match self {
            Self::Any => 0,
            Self::Link => 1,
            Self::Host => 2,
            Self::Nowhere => 3,
        }
    }

    fn matches(self, scope: u8) -> bool {
        match self {
            Self::Any => true,
            Self::Link => scope >= RT_SCOPE_LINK,
            Self::Host => scope >= RT_SCOPE_HOST,
            Self::Nowhere => scope == RT_SCOPE_NOWHERE,
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct ScopeWinners([Option<usize>; ScopeRequirement::ALL.len()]);

impl ScopeWinners {
    fn get(self, requirement: ScopeRequirement) -> Option<usize> {
        self.0[requirement.index()]
    }

    fn references(self, route_index: usize) -> bool {
        self.0.contains(&Some(route_index))
    }

    fn replace_index(&mut self, old_index: usize, new_index: usize) {
        for winner in &mut self.0 {
            if *winner == Some(old_index) {
                *winner = Some(new_index);
            }
        }
    }

    fn consider(
        &mut self,
        route_index: usize,
        route: RouteEntry,
        routes: &[RouteEntry],
        alias_order: &[AliasOrder],
    ) {
        for requirement in ScopeRequirement::ALL {
            if !requirement.matches(route.scope) {
                continue;
            }
            let winner = &mut self.0[requirement.index()];
            if winner
                .is_none_or(|current| route_precedes(route_index, current, routes, alias_order))
            {
                *winner = Some(route_index);
            }
        }
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
struct CandidateWinners {
    any_oif: ScopeWinners,
    by_oif: HashMap<u32, ScopeWinners>,
}

impl CandidateWinners {
    fn is_empty(&self) -> bool {
        self.any_oif == ScopeWinners::default() && self.by_oif.is_empty()
    }

    fn clear(&mut self) {
        self.any_oif = ScopeWinners::default();
        self.by_oif.clear();
    }

    fn references(&self, route_index: usize) -> bool {
        self.any_oif.references(route_index)
            || self
                .by_oif
                .values()
                .any(|winners| winners.references(route_index))
    }

    fn replace_index(&mut self, old_index: usize, new_index: usize) {
        self.any_oif.replace_index(old_index, new_index);
        for winners in self.by_oif.values_mut() {
            winners.replace_index(old_index, new_index);
        }
    }

    fn get(&self, oif: Option<u32>, requirement: ScopeRequirement) -> Option<usize> {
        match oif {
            Some(oif) => self.by_oif.get(&oif).copied(),
            None => Some(self.any_oif),
        }
        .and_then(|winners| winners.get(requirement))
    }

    fn consider(
        &mut self,
        route_index: usize,
        route: RouteEntry,
        routes: &[RouteEntry],
        alias_order: &[AliasOrder],
    ) {
        self.any_oif
            .consider(route_index, route, routes, alias_order);
        self.by_oif
            .entry(route.oif)
            .or_default()
            .consider(route_index, route, routes, alias_order);
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
struct SourceWinners {
    // Bit n denotes a committed IPv6 source prefix of length n + 1.
    lengths: u128,
    by_prefix: HashMap<IpCidr, CandidateWinners>,
}

enum SourceLookup {
    NoMatch,
    NoEligibleRoute,
    Found(usize),
}

impl SourceWinners {
    fn lookup(
        &self,
        source: Ipv6Address,
        required_oif: Option<u32>,
        scope: ScopeRequirement,
    ) -> SourceLookup {
        let mut lengths = self.lengths;
        let mut matched = false;
        while lengths != 0 {
            let length = (u128::BITS - lengths.leading_zeros()) as u8;
            lengths &= !(1 << (length - 1));
            let key = canonical_cidr(IpCidr::new(source.into(), length));
            let Some(candidates) = self.by_prefix.get(&key) else {
                continue;
            };
            matched = true;
            if let Some(route) = candidates.get(required_oif, scope) {
                return SourceLookup::Found(route);
            }
        }
        if matched {
            SourceLookup::NoEligibleRoute
        } else {
            SourceLookup::NoMatch
        }
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
struct PrefixWinners {
    standard: CandidateWinners,
    broadcast: CandidateWinners,
    source: Option<Box<SourceWinners>>,
    ipv6_serial: u64,
}

impl PrefixWinners {
    fn clear(&mut self) {
        self.standard.clear();
        self.broadcast.clear();
        if let Some(source) = &mut self.source {
            source.lengths = 0;
            for candidates in source.by_prefix.values_mut() {
                candidates.clear();
            }
        }
    }

    fn is_empty(&self) -> bool {
        self.standard.is_empty() && self.broadcast.is_empty() && self.source.is_none()
    }

    fn remove_empty_sources(&mut self) {
        if let Some(source) = &mut self.source {
            source
                .by_prefix
                .retain(|_, candidates| !candidates.is_empty());
            if source.by_prefix.is_empty() {
                self.source = None;
            }
        }
    }

    fn cancel_source_reservation(&mut self, route: RouteEntry) {
        if let (Some(prefix), Some(source)) = (route.source, &mut self.source) {
            if source
                .by_prefix
                .get(&prefix)
                .is_some_and(CandidateWinners::is_empty)
            {
                source.by_prefix.remove(&prefix);
            }
            if source.by_prefix.is_empty() {
                self.source = None;
            }
        }
    }

    fn references(&self, route: RouteEntry, route_index: usize) -> bool {
        if let Some(prefix) = route.source {
            self.source
                .as_ref()
                .and_then(|source| source.by_prefix.get(&prefix))
                .is_some_and(|candidates| candidates.references(route_index))
        } else if route.kind == RTN_BROADCAST {
            self.broadcast.references(route_index)
        } else {
            self.standard.references(route_index)
        }
    }

    fn replace_index(&mut self, route: RouteEntry, old_index: usize, new_index: usize) {
        if let Some(prefix) = route.source {
            if let Some(candidates) = self
                .source
                .as_mut()
                .and_then(|source| source.by_prefix.get_mut(&prefix))
            {
                candidates.replace_index(old_index, new_index);
            }
        } else if route.kind == RTN_BROADCAST {
            self.broadcast.replace_index(old_index, new_index);
        } else {
            self.standard.replace_index(old_index, new_index);
        }
    }

    fn destination_candidates_mut(&mut self, route: RouteEntry) -> &mut CandidateWinners {
        if route.kind == RTN_BROADCAST {
            &mut self.broadcast
        } else {
            &mut self.standard
        }
    }

    fn reserve_candidate(&mut self, route: RouteEntry) -> Result<(), SystemError> {
        let candidates = if let Some(prefix) = route.source {
            if self.source.is_none() {
                self.source =
                    Some(Box::try_new(SourceWinners::default()).map_err(|_| SystemError::ENOMEM)?);
            }
            let source = self.source.as_mut().unwrap();
            if !source.by_prefix.contains_key(&prefix) {
                source
                    .by_prefix
                    .try_reserve(1)
                    .map_err(|_| SystemError::ENOMEM)?;
            }
            source.by_prefix.entry(prefix).or_default()
        } else {
            self.destination_candidates_mut(route)
        };
        candidates
            .by_oif
            .try_reserve(1)
            .map_err(|_| SystemError::ENOMEM)
    }

    fn consider(
        &mut self,
        index: usize,
        route: RouteEntry,
        routes: &[RouteEntry],
        orders: &[AliasOrder],
    ) {
        let candidates = if let Some(prefix) = route.source {
            let source = self
                .source
                .as_mut()
                .expect("source index must be reserved before commit");
            source.lengths |= 1 << (prefix.prefix_len() - 1);
            source
                .by_prefix
                .get_mut(&prefix)
                .expect("source prefix must be reserved before commit")
        } else {
            self.destination_candidates_mut(route)
        };
        candidates.consider(index, route, routes, orders);
    }
}

impl PrefixKey {
    fn new(table: u32, destination: IpCidr) -> Self {
        let destination = canonical_cidr(destination);
        Self {
            table,
            family: AddressFamily::of(destination.address()),
            destination: destination.address(),
            prefix_len: destination.prefix_len(),
        }
    }

    fn for_lookup(table: u32, destination: IpAddress, prefix_len: u8) -> Self {
        Self::new(table, IpCidr::new(destination, prefix_len))
    }
}

#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq)]
pub(super) struct ProjectionKey {
    oif: u32,
    family: AddressFamily,
    destination: IpAddress,
    prefix_len: u8,
}

impl ProjectionKey {
    pub(super) fn new(oif: u32, destination: IpCidr) -> Self {
        let destination = canonical_cidr(destination);
        Self {
            oif,
            family: AddressFamily::of(destination.address()),
            destination: destination.address(),
            prefix_len: destination.prefix_len(),
        }
    }

    pub(super) fn cidr(self) -> IpCidr {
        IpCidr::new(self.destination, self.prefix_len)
    }

    pub(super) fn oif(self) -> u32 {
        self.oif
    }

    pub(super) fn is_ipv6(self) -> bool {
        self.family == AddressFamily::Ipv6
    }
}

/// Derived lookup and projection indexes for the authoritative route vector.
///
/// `all` buckets contain authoritative vector positions in insertion order.
/// `winners` preselects each packet-path selector. An expired learned route
/// can require inspecting the same prefix for a live alternative.
/// Lifecycle edits rebuild only affected prefix winners.
/// `alias_order` keeps semantic alias order independent of authoritative
/// vector slots moved by `swap_remove`, without shifting a whole prefix on
/// prepend.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct FibIndex {
    all: HashMap<PrefixKey, Vec<usize>>,
    alias: AliasIndex,
    winners: HashMap<PrefixKey, PrefixWinners>,
    projection: HashMap<ProjectionKey, Vec<usize>>,
    projection_winners: HashMap<ProjectionKey, usize>,
    prefix_counts: HashMap<PrefixDomain, Vec<PrefixLengthCount>>,
    source_route_count: usize,
    ipv6_special_route_count: usize,
    ipv6_serial: u64,
}

impl FibIndex {
    #[cfg(test)]
    pub(super) fn build(routes: &[RouteEntry]) -> Result<Self, SystemError> {
        let mut index = Self::default();
        for (route_index, route) in routes.iter().copied().enumerate() {
            index.prepare_insert(route)?;
            // Test rebuilds receive routes in semantic alias order.
            let placement = index.append_placement(route);
            index.commit_insert(route_index, route, placement, routes);
        }
        Ok(index)
    }

    pub(super) fn try_clone(&self) -> Result<Self, SystemError> {
        Ok(Self {
            all: try_clone_buckets(&self.all)?,
            alias: self.alias.try_clone()?,
            winners: try_clone_winners(&self.winners)?,
            projection: try_clone_buckets(&self.projection)?,
            projection_winners: try_clone_map(&self.projection_winners)?,
            prefix_counts: try_clone_prefix_counts(&self.prefix_counts)?,
            source_route_count: self.source_route_count,
            ipv6_special_route_count: self.ipv6_special_route_count,
            ipv6_serial: self.ipv6_serial,
        })
    }

    pub(super) fn has_source_routes(&self) -> bool {
        self.source_route_count != 0
    }

    pub(super) fn has_ipv6_special_routes(&self) -> bool {
        self.ipv6_special_route_count != 0
    }

    fn update_route_counts(&mut self, route: RouteEntry, insert: bool) {
        let source = usize::from(route.source.is_some());
        let special = usize::from(ipv6_special(route));
        if insert {
            self.source_route_count += source;
            self.ipv6_special_route_count += special;
        } else {
            self.source_route_count -= source;
            self.ipv6_special_route_count -= special;
        }
    }

    pub(super) fn ipv6_serial(&self, route: RouteEntry) -> u64 {
        let key = PrefixKey::new(route.table, route.destination);
        self.winners
            .get(&key)
            .map_or(0, |winners| winners.ipv6_serial)
    }

    pub(super) fn ipv6_cookie_winner(
        &self,
        routes: &[RouteEntry],
        destination: IpAddress,
        table: u32,
        prefix_len: u8,
        serial: u64,
        oif: Option<u32>,
    ) -> Option<usize> {
        let key = PrefixKey::for_lookup(table, destination, prefix_len);
        let winners = self.winners.get(&key)?;
        if winners.ipv6_serial != serial {
            return None;
        }
        let scope = ScopeRequirement::Any;
        if let Some(source) = &winners.source {
            match source.lookup(Ipv6Address::UNSPECIFIED, oif, scope) {
                SourceLookup::Found(route) => return Some(route),
                SourceLookup::NoMatch => return None,
                SourceLookup::NoEligibleRoute => {}
            }
        }
        let route = winners.standard.get(oif, scope)?;
        // Do not substitute another alias under a cookie for an expired
        // route. The normal lookup after this miss uses the actual source.
        routes[route]
            .expires_at_us()
            .is_none_or(|expires| expires > crate::time::Instant::now().total_micros())
            .then_some(route)
    }

    fn touch_ipv6_prefix(&mut self, key: PrefixKey) {
        if key.family != AddressFamily::Ipv6 {
            return;
        }
        self.ipv6_serial += 1;
        set_ipv6_prefix_serial(
            &mut self.winners,
            &self.prefix_counts,
            key,
            self.ipv6_serial,
        );
    }

    pub(super) fn has_ipv4_routes(&self, table: u32) -> bool {
        self.prefix_counts.contains_key(&PrefixDomain {
            table,
            family: AddressFamily::Ipv4,
        })
    }

    pub(super) fn has_ipv6_routes(&self, table: u32) -> bool {
        self.prefix_counts.contains_key(&PrefixDomain {
            table,
            family: AddressFamily::Ipv6,
        })
    }

    /// Reserve every bucket needed by a later infallible commit.
    pub(super) fn prepare_insert(&mut self, route: RouteEntry) -> Result<(), SystemError> {
        let result = (|| {
            self.alias.prepare_insert(route)?;
            reserve_bucket(
                &mut self.all,
                PrefixKey::new(route.table, route.destination),
            )?;
            if indexable(route) {
                reserve_prefix_domain(&mut self.prefix_counts, route)?;
                reserve_winner(&mut self.winners, route)?;
            }
            if let Some(key) = projection_key(route) {
                reserve_bucket(&mut self.projection, key)?;
                if !self.projection_winners.contains_key(&key) {
                    self.projection_winners
                        .try_reserve(1)
                        .map_err(|_| SystemError::ENOMEM)?;
                }
            }
            Ok(())
        })();
        if result.is_err() {
            self.cancel_insert_reservation(route);
        }
        result
    }

    /// Remove empty buckets left by a preparation that was not committed.
    pub(super) fn cancel_insert_reservation(&mut self, route: RouteEntry) {
        remove_empty_bucket(
            &mut self.all,
            &PrefixKey::new(route.table, route.destination),
        );
        if indexable(route) {
            let key = PrefixKey::new(route.table, route.destination);
            if let Some(winners) = self.winners.get_mut(&key) {
                winners.cancel_source_reservation(route);
            }
            remove_empty_winner(&mut self.winners, &key);
        }
        if let Some(key) = projection_key(route) {
            remove_empty_bucket(&mut self.projection, &key);
        }
        remove_empty_prefix_domain(&mut self.prefix_counts, route);
    }

    pub(super) fn commit_insert(
        &mut self,
        route_index: usize,
        route: RouteEntry,
        placement: AliasPlacement,
        routes: &[RouteEntry],
    ) {
        let prefix = PrefixKey::new(route.table, route.destination);
        self.alias.commit_insert(route_index, route, placement);
        insert_reserved_bucket(&mut self.all, prefix, route_index, None);
        if indexable(route) {
            self.update_route_counts(route, true);
            update_prefix_count(&mut self.prefix_counts, route, true);
            self.touch_ipv6_prefix(prefix);
            // Existing winners remain valid across insertion: relative order
            // between existing aliases is unchanged, so only the new route
            // can displace them. Deletion/replacement still rebuilds when a
            // cached winner may disappear or change class.
            self.winners
                .get_mut(&prefix)
                .expect("winner prefix must be reserved before commit")
                .consider(route_index, route, routes, self.alias.orders());
        }
        if let Some(key) = projection_key(route) {
            insert_reserved_bucket(&mut self.projection, key, route_index, None);
            let replace = self
                .projection_winners
                .get(&key)
                .copied()
                .is_none_or(|current| {
                    projection_route_precedes(route_index, current, routes, self.alias.orders())
                });
            if replace {
                self.projection_winners.insert(key, route_index);
            }
        }
    }

    pub(super) fn commit_remove(
        &mut self,
        route_index: usize,
        route: RouteEntry,
        moved: Option<(usize, RouteEntry)>,
        routes: &[RouteEntry],
    ) {
        let prefix = PrefixKey::new(route.table, route.destination);
        let removed_was_winner = self
            .winners
            .get(&prefix)
            .is_some_and(|winners| winners.references(route, route_index));
        let projection = projection_key(route);
        let removed_was_projection_winner =
            projection.is_some_and(|key| self.projection_winners.get(&key) == Some(&route_index));
        remove_index(
            &mut self.all,
            &PrefixKey::new(route.table, route.destination),
            route_index,
        );
        if let Some(key) = projection {
            remove_index(&mut self.projection, &key, route_index);
        }
        self.alias.commit_remove(
            route_index,
            route,
            self.all.get(&prefix).map(Vec::as_slice).unwrap_or_default(),
        );
        if indexable(route) {
            self.touch_ipv6_prefix(prefix);
            self.update_route_counts(route, false);
            update_prefix_count(&mut self.prefix_counts, route, false);
            remove_empty_prefix_domain(&mut self.prefix_counts, route);
        }
        if let Some((old_index, moved_route)) = moved {
            self.replace_route_index(old_index, route_index, moved_route);
        }
        if removed_was_winner {
            self.rebuild_winners(prefix, routes);
        }
        if let Some(key) = projection.filter(|_| removed_was_projection_winner) {
            self.rebuild_projection_winner(key, routes);
        }
    }

    /// Apply a stable compaction of the authoritative route vector.
    ///
    /// `old_to_new` maps every old vector slot to its retained slot, or to
    /// `None` when the route was removed. Bucket order is preserved, so exact
    /// prefix aliases keep their semantic insertion order even when earlier
    /// single-route removals have made physical vector order differ from it.
    pub(super) fn commit_retain(&mut self, old_to_new: &[Option<usize>], routes: &[RouteEntry]) {
        debug_assert_eq!(
            old_to_new.iter().filter(|index| index.is_some()).count(),
            routes.len()
        );

        // Inspect the old buckets before their route indices are remapped.
        // All affected IPv6 prefixes in this transaction share one serial.
        let serial = self.ipv6_serial + 1;
        for (&key, indices) in &self.all {
            if key.family == AddressFamily::Ipv6
                && indices.iter().any(|&index| old_to_new[index].is_none())
            {
                set_ipv6_prefix_serial(&mut self.winners, &self.prefix_counts, key, serial);
                self.ipv6_serial = serial;
            }
        }
        remap_retained_indices(&mut self.all, old_to_new);
        remap_retained_indices(&mut self.projection, old_to_new);

        self.alias.commit_retain(old_to_new, routes);

        for counts in self.prefix_counts.values_mut() {
            counts.clear();
        }
        self.source_route_count = 0;
        self.ipv6_special_route_count = 0;
        for route in routes.iter().copied().filter(|route| indexable(*route)) {
            self.update_route_counts(route, true);
            update_prefix_count(&mut self.prefix_counts, route, true);
        }
        self.prefix_counts.retain(|_, counts| !counts.is_empty());
        self.rebuild_all_winners(routes);
        self.rebuild_all_projection_winners(routes);
    }

    pub(super) fn commit_replace(
        &mut self,
        route_index: usize,
        old: RouteEntry,
        new: RouteEntry,
        routes: &[RouteEntry],
    ) {
        let old_prefix = PrefixKey::new(old.table, old.destination);
        let new_prefix = PrefixKey::new(new.table, new.destination);
        debug_assert_eq!(old_prefix, new_prefix);

        let old_indexable = indexable(old);
        let new_indexable = indexable(new);
        if old_indexable {
            self.update_route_counts(old, false);
        }
        if new_indexable {
            self.update_route_counts(new, true);
        }
        let old_was_winner = self
            .winners
            .get(&old_prefix)
            .is_some_and(|winners| winners.references(old, route_index));
        if old_indexable != new_indexable {
            update_prefix_count(
                &mut self.prefix_counts,
                if old_indexable { old } else { new },
                new_indexable,
            );
            remove_empty_prefix_domain(&mut self.prefix_counts, old);
        }
        if old_indexable || new_indexable {
            self.touch_ipv6_prefix(new_prefix);
        }

        let old_projection = projection_key(old);
        let new_projection = projection_key(new);
        let old_was_projection_winner = old_projection
            .is_some_and(|key| self.projection_winners.get(&key) == Some(&route_index));
        self.alias.commit_replace(route_index, old, new);
        if old_projection != new_projection {
            if let Some(key) = old_projection {
                remove_index(&mut self.projection, &key, route_index);
            }
            if let Some(key) = new_projection {
                insert_reserved_bucket(&mut self.projection, key, route_index, None);
            }
        }
        if old_was_winner {
            self.rebuild_winners(new_prefix, routes);
        } else if new_indexable {
            self.winners
                .get_mut(&new_prefix)
                .expect("winner prefix must be reserved before replace")
                .consider(route_index, new, routes, self.alias.orders());
        }
        if old_projection != new_projection {
            if let Some(key) = old_projection.filter(|_| old_was_projection_winner) {
                self.rebuild_projection_winner(key, routes);
            }
            if let Some(key) = new_projection {
                let replace = self
                    .projection_winners
                    .get(&key)
                    .copied()
                    .is_none_or(|current| {
                        projection_route_precedes(route_index, current, routes, self.alias.orders())
                    });
                if replace {
                    self.projection_winners.insert(key, route_index);
                }
            }
        }
    }

    pub(super) fn analyze_insert(&self, route: RouteEntry) -> AliasInsertAnalysis {
        self.alias.analyze_insert(route)
    }

    pub(super) fn append_placement(&self, route: RouteEntry) -> AliasPlacement {
        self.alias.append_placement(route)
    }

    pub(super) fn planned_order(&self, route: RouteEntry, placement: AliasPlacement) -> AliasOrder {
        self.alias.planned_order(route, placement)
    }

    pub(super) fn projection_winner(&self, key: ProjectionKey) -> Option<usize> {
        self.projection_winners.get(&key).copied()
    }

    pub(super) fn projected_insert_precedes(
        &self,
        route: RouteEntry,
        placement: AliasPlacement,
        current_index: usize,
        current: RouteEntry,
    ) -> bool {
        (
            u8::from(route.table != RT_TABLE_LOCAL),
            self.planned_order(route, placement),
        ) < (
            u8::from(current.table != RT_TABLE_LOCAL),
            self.alias.order(current_index),
        )
    }

    pub(super) fn prefix_candidates(&self, route: RouteEntry) -> &[usize] {
        self.prefix_candidates_for(route.table, route.destination)
    }

    pub(super) fn prefix_candidates_for(&self, table: u32, destination: IpCidr) -> &[usize] {
        self.all
            .get(&PrefixKey::new(table, destination))
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    pub(super) fn projection_candidates(&self, key: ProjectionKey) -> &[usize] {
        self.projection
            .get(&key)
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    pub(super) fn order_key(&self, route_index: usize) -> AliasOrder {
        self.alias.order(route_index)
    }

    pub(super) fn projection_for_iface(
        &self,
        routes: &[RouteEntry],
        ifindex: u32,
    ) -> Result<Vec<SmolRoute>, SystemError> {
        let count = self
            .projection_winners
            .keys()
            .filter(|key| key.oif == ifindex)
            .count();
        let mut projection = Vec::new();
        projection
            .try_reserve_exact(count)
            .map_err(|_| SystemError::ENOMEM)?;
        let mut alternatives = Vec::new();
        for (key, winner) in self
            .projection_winners
            .iter()
            .filter(|(key, _)| key.oif == ifindex)
        {
            if key.is_ipv6() {
                let candidates = self
                    .projection_candidates(*key)
                    .iter()
                    .map(|&index| (routes[index], self.order_key(index)));
                if let Some(group) = learned_projection(candidates)? {
                    alternatives
                        .try_reserve(group.len())
                        .map_err(|_| SystemError::ENOMEM)?;
                    alternatives.extend(group);
                    continue;
                }
            }
            let route = routes[*winner];
            projection.push(smol_route(route));
        }
        projection.sort_unstable_by_key(|route| route.cidr);
        projection
            .try_reserve(alternatives.len())
            .map_err(|_| SystemError::ENOMEM)?;
        projection.extend(alternatives);
        Ok(projection)
    }

    /// Visits exact-prefix buckets from longest to shortest. Work is bounded
    /// by the address-family prefix width rather than by route-table size.
    #[cfg(test)]
    fn lookup(
        &self,
        routes: &[RouteEntry],
        destination: IpAddress,
        table: u32,
        required_oif: Option<u32>,
        minimum_scope: Option<u8>,
        broadcast: BroadcastLookup,
    ) -> Option<usize> {
        self.lookup_key(
            routes,
            FibLookupKey {
                destination,
                source: None,
                table,
                required_oif,
                minimum_scope,
                broadcast,
            },
        )
    }

    pub(super) fn lookup_key(&self, routes: &[RouteEntry], query: FibLookupKey) -> Option<usize> {
        let FibLookupKey {
            destination,
            source,
            table,
            required_oif,
            minimum_scope,
            broadcast,
        } = query;
        let domain = PrefixDomain {
            table,
            family: AddressFamily::of(destination),
        };
        let counts = self.prefix_counts.get(&domain)?;
        let scope = ScopeRequirement::from_minimum(minimum_scope)?;
        for prefix_len in counts.iter().map(|entry| entry.prefix_len) {
            let key = PrefixKey::for_lookup(table, destination, prefix_len);
            let Some(winners) = self.winners.get(&key) else {
                continue;
            };
            if let Some(sources) = &winners.source {
                match sources.lookup(
                    source.unwrap_or(Ipv6Address::UNSPECIFIED),
                    required_oif,
                    scope,
                ) {
                    SourceLookup::Found(route) => return Some(route),
                    SourceLookup::NoMatch => continue,
                    SourceLookup::NoEligibleRoute => {}
                }
            }
            let mut standard = winners.standard.get(required_oif, scope);
            if standard.is_some_and(|index| routes[index].expires_at_us().is_some()) {
                let now_us = crate::time::Instant::now().total_micros();
                if standard.is_some_and(|index| !routes[index].is_live_at(now_us)) {
                    standard = self.all[&key]
                        .iter()
                        .copied()
                        .filter(|&index| {
                            let route = routes[index];
                            indexable(route)
                                && route.source.is_none()
                                && route.kind != RTN_BROADCAST
                                && required_oif.is_none_or(|oif| oif == route.oif)
                                && scope.matches(route.scope)
                                && route.is_live_at(now_us)
                        })
                        .min_by_key(|&index| route_order(routes[index], self.alias.order(index)));
                }
            }
            let broadcast = match broadcast {
                BroadcastLookup::Exclude => None,
                BroadcastLookup::Any => winners.broadcast.get(None, scope),
                BroadcastLookup::OnIface(oif) => winners.broadcast.get(Some(oif), scope),
            };
            match (standard, broadcast) {
                (Some(left), Some(right)) => {
                    return Some(self.preferred_route(routes, left, right));
                }
                (Some(route), None) | (None, Some(route)) => return Some(route),
                (None, None) => {}
            }
        }
        None
    }

    fn preferred_route(&self, routes: &[RouteEntry], left: usize, right: usize) -> usize {
        let left_route = routes[left];
        let right_route = routes[right];
        let left_is_preferred = left_route
            .destination
            .prefix_len()
            .cmp(&right_route.destination.prefix_len())
            .then_with(|| right_route.priority.cmp(&left_route.priority))
            .then_with(|| self.alias.order(right).cmp(&self.alias.order(left)))
            .is_ge();
        if left_is_preferred {
            left
        } else {
            right
        }
    }

    fn replace_route_index(&mut self, old_index: usize, new_index: usize, route: RouteEntry) {
        self.alias.remap_moved(old_index, new_index, route);
        replace_index(
            &mut self.all,
            &PrefixKey::new(route.table, route.destination),
            old_index,
            new_index,
        );
        if let Some(key) = projection_key(route) {
            replace_index(&mut self.projection, &key, old_index, new_index);
            if self.projection_winners.get(&key) == Some(&old_index) {
                self.projection_winners.insert(key, new_index);
            }
        }
        if let Some(winners) = self
            .winners
            .get_mut(&PrefixKey::new(route.table, route.destination))
        {
            winners.replace_index(route, old_index, new_index);
        }
    }

    fn rebuild_projection_winner(&mut self, key: ProjectionKey, routes: &[RouteEntry]) {
        let winner = self.projection.get(&key).and_then(|indices| {
            indices.iter().copied().min_by(|left, right| {
                projection_order(*left, routes, self.alias.orders()).cmp(&projection_order(
                    *right,
                    routes,
                    self.alias.orders(),
                ))
            })
        });
        match winner {
            Some(winner) => {
                self.projection_winners.insert(key, winner);
            }
            None => {
                self.projection_winners.remove(&key);
            }
        }
    }

    fn rebuild_all_projection_winners(&mut self, routes: &[RouteEntry]) {
        self.projection_winners.clear();
        for (key, indices) in &self.projection {
            let winner = indices
                .iter()
                .copied()
                .min_by_key(|index| projection_order(*index, routes, self.alias.orders()))
                .expect("projection buckets cannot be empty");
            self.projection_winners.insert(*key, winner);
        }
    }

    fn rebuild_winners(&mut self, prefix: PrefixKey, routes: &[RouteEntry]) {
        let Some(winners) = self.winners.get_mut(&prefix) else {
            debug_assert!(self
                .all
                .get(&prefix)
                .is_none_or(|indices| indices.iter().all(|index| !indexable(routes[*index]))));
            return;
        };
        winners.clear();
        if let Some(indices) = self.all.get(&prefix) {
            for route_index in indices.iter().copied() {
                let route = routes[route_index];
                if indexable(route) {
                    winners.consider(route_index, route, routes, self.alias.orders());
                }
            }
        }
        winners.remove_empty_sources();
        if winners.is_empty() {
            self.winners.remove(&prefix);
        }
    }

    fn rebuild_all_winners(&mut self, routes: &[RouteEntry]) {
        for winners in self.winners.values_mut() {
            winners.clear();
        }
        for (prefix, indices) in &self.all {
            let Some(winners) = self.winners.get_mut(prefix) else {
                debug_assert!(indices.iter().all(|index| !indexable(routes[*index])));
                continue;
            };
            for route_index in indices.iter().copied() {
                let route = routes[route_index];
                if indexable(route) {
                    winners.consider(route_index, route, routes, self.alias.orders());
                }
            }
        }
        self.winners.retain(|_, winners| {
            winners.remove_empty_sources();
            !winners.is_empty()
        });
    }
}

fn try_clone_buckets<K>(
    source: &HashMap<K, Vec<usize>>,
) -> Result<HashMap<K, Vec<usize>>, SystemError>
where
    K: Copy + Eq + core::hash::Hash,
{
    let mut cloned = HashMap::new();
    cloned
        .try_reserve(source.len())
        .map_err(|_| SystemError::ENOMEM)?;
    for (key, indices) in source {
        let mut copied = Vec::new();
        copied
            .try_reserve_exact(indices.len())
            .map_err(|_| SystemError::ENOMEM)?;
        copied.extend_from_slice(indices);
        cloned.insert(*key, copied);
    }
    Ok(cloned)
}

fn try_clone_map<K, V>(source: &HashMap<K, V>) -> Result<HashMap<K, V>, SystemError>
where
    K: Copy + Eq + core::hash::Hash,
    V: Copy,
{
    let mut cloned = HashMap::new();
    cloned
        .try_reserve(source.len())
        .map_err(|_| SystemError::ENOMEM)?;
    cloned.extend(source.iter().map(|(key, value)| (*key, *value)));
    Ok(cloned)
}

/// Linux IPv6 invalidates cached routes through the changed node's ancestors.
/// Only buckets that can currently supply a route need a serial.
fn set_ipv6_prefix_serial(
    winners: &mut HashMap<PrefixKey, PrefixWinners>,
    prefix_counts: &HashMap<PrefixDomain, Vec<PrefixLengthCount>>,
    changed: PrefixKey,
    serial: u64,
) {
    let domain = PrefixDomain {
        table: changed.table,
        family: AddressFamily::Ipv6,
    };
    let Some(prefixes) = prefix_counts.get(&domain) else {
        return;
    };
    for length in prefixes
        .iter()
        .filter_map(|entry| (entry.prefix_len <= changed.prefix_len).then_some(entry.prefix_len))
    {
        let key = PrefixKey::for_lookup(changed.table, changed.destination, length);
        if let Some(winner) = winners.get_mut(&key) {
            winner.ipv6_serial = serial;
        }
    }
}

fn try_clone_winners(
    source: &HashMap<PrefixKey, PrefixWinners>,
) -> Result<HashMap<PrefixKey, PrefixWinners>, SystemError> {
    let mut cloned = HashMap::new();
    cloned
        .try_reserve(source.len())
        .map_err(|_| SystemError::ENOMEM)?;
    for (prefix, winners) in source {
        cloned.insert(
            *prefix,
            PrefixWinners {
                standard: try_clone_candidate_winners(&winners.standard)?,
                broadcast: try_clone_candidate_winners(&winners.broadcast)?,
                source: winners
                    .source
                    .as_deref()
                    .map(try_clone_source_winners)
                    .transpose()?,
                ipv6_serial: winners.ipv6_serial,
            },
        );
    }
    Ok(cloned)
}

fn try_clone_source_winners(source: &SourceWinners) -> Result<Box<SourceWinners>, SystemError> {
    let mut by_prefix = HashMap::new();
    by_prefix
        .try_reserve(source.by_prefix.len())
        .map_err(|_| SystemError::ENOMEM)?;
    for (prefix, candidates) in &source.by_prefix {
        by_prefix.insert(*prefix, try_clone_candidate_winners(candidates)?);
    }
    Box::try_new(SourceWinners {
        lengths: source.lengths,
        by_prefix,
    })
    .map_err(|_| SystemError::ENOMEM)
}

fn try_clone_candidate_winners(source: &CandidateWinners) -> Result<CandidateWinners, SystemError> {
    let mut by_oif = HashMap::new();
    by_oif
        .try_reserve(source.by_oif.len())
        .map_err(|_| SystemError::ENOMEM)?;
    by_oif.extend(source.by_oif.iter().map(|(oif, winners)| (*oif, *winners)));
    Ok(CandidateWinners {
        any_oif: source.any_oif,
        by_oif,
    })
}

fn try_clone_prefix_counts(
    source: &HashMap<PrefixDomain, Vec<PrefixLengthCount>>,
) -> Result<HashMap<PrefixDomain, Vec<PrefixLengthCount>>, SystemError> {
    let mut cloned = HashMap::new();
    cloned
        .try_reserve(source.len())
        .map_err(|_| SystemError::ENOMEM)?;
    for (domain, lengths) in source {
        let mut copied = Vec::new();
        copied
            .try_reserve_exact(lengths.len())
            .map_err(|_| SystemError::ENOMEM)?;
        copied.extend_from_slice(lengths);
        cloned.insert(*domain, copied);
    }
    Ok(cloned)
}

fn reserve_bucket<K>(buckets: &mut HashMap<K, Vec<usize>>, key: K) -> Result<(), SystemError>
where
    K: Copy + Eq + core::hash::Hash,
{
    if let Some(indices) = buckets.get_mut(&key) {
        return indices.try_reserve(1).map_err(|_| SystemError::ENOMEM);
    }
    buckets.try_reserve(1).map_err(|_| SystemError::ENOMEM)?;
    let mut indices = Vec::new();
    indices
        .try_reserve_exact(1)
        .map_err(|_| SystemError::ENOMEM)?;
    buckets.insert(key, indices);
    Ok(())
}

fn reserve_winner(
    winners: &mut HashMap<PrefixKey, PrefixWinners>,
    route: RouteEntry,
) -> Result<(), SystemError> {
    let prefix = PrefixKey::new(route.table, route.destination);
    if !winners.contains_key(&prefix) {
        winners.try_reserve(1).map_err(|_| SystemError::ENOMEM)?;
        winners.insert(prefix, PrefixWinners::default());
    }
    winners
        .get_mut(&prefix)
        .expect("winner prefix was just reserved")
        .reserve_candidate(route)
}

fn remove_empty_winner(winners: &mut HashMap<PrefixKey, PrefixWinners>, prefix: &PrefixKey) {
    if winners.get(prefix).is_some_and(PrefixWinners::is_empty) {
        winners.remove(prefix);
    }
}

fn insert_reserved_bucket<K>(
    buckets: &mut HashMap<K, Vec<usize>>,
    key: K,
    route_index: usize,
    before: Option<usize>,
) where
    K: Copy + Eq + core::hash::Hash,
{
    let indices = buckets
        .get_mut(&key)
        .expect("route index bucket must be reserved before commit");
    let position = before
        .and_then(|before| indices.iter().position(|index| *index == before))
        .unwrap_or(indices.len());
    indices.insert(position, route_index);
}

fn remove_index<K>(buckets: &mut HashMap<K, Vec<usize>>, key: &K, route_index: usize)
where
    K: Copy + Eq + core::hash::Hash,
{
    let remove_bucket = if let Some(indices) = buckets.get_mut(key) {
        let position = indices
            .iter()
            .position(|index| *index == route_index)
            .expect("authoritative route must have a matching index entry");
        indices.remove(position);
        indices.is_empty()
    } else {
        false
    };
    if remove_bucket {
        buckets.remove(key);
    }
}

fn remove_empty_bucket<K>(buckets: &mut HashMap<K, Vec<usize>>, key: &K)
where
    K: Copy + Eq + core::hash::Hash,
{
    if buckets.get(key).is_some_and(Vec::is_empty) {
        buckets.remove(key);
    }
}

fn replace_index<K>(
    buckets: &mut HashMap<K, Vec<usize>>,
    key: &K,
    old_index: usize,
    new_index: usize,
) where
    K: Copy + Eq + core::hash::Hash,
{
    let indices = buckets
        .get_mut(key)
        .expect("moved authoritative route must have an index bucket");
    let index = indices
        .iter()
        .position(|index| *index == old_index)
        .expect("moved authoritative route must have an index entry");
    indices[index] = new_index;
}

fn remap_retained_indices<K>(buckets: &mut HashMap<K, Vec<usize>>, old_to_new: &[Option<usize>])
where
    K: Copy + Eq + core::hash::Hash,
{
    buckets.retain(|_, indices| {
        let mut retained = 0;
        for old_index in 0..indices.len() {
            if let Some(new_index) = old_to_new[indices[old_index]] {
                indices[retained] = new_index;
                retained += 1;
            }
        }
        indices.truncate(retained);
        retained != 0
    });
}

fn route_precedes(
    candidate: usize,
    current: usize,
    routes: &[RouteEntry],
    alias_order: &[AliasOrder],
) -> bool {
    route_order(routes[candidate], alias_order[candidate])
        < route_order(routes[current], alias_order[current])
}

fn route_order(route: RouteEntry, order: AliasOrder) -> (u32, RouterPreference, AliasOrder) {
    (route.priority, route.preference(), order)
}

pub(super) fn smol_route(route: RouteEntry) -> SmolRoute {
    SmolRoute {
        cidr: canonical_cidr(route.destination),
        via_router: route.gateway,
        preferred_until: None,
        expires_at: route.expires_at_us().map(|deadline| {
            // smoltcp stores the last usable microsecond, while the FIB
            // stores the first expired microsecond.
            smoltcp::time::Instant::from_micros(deadline.saturating_sub(1))
        }),
    }
}

pub(super) fn learned_projection(
    candidates: impl Iterator<Item = (RouteEntry, AliasOrder)> + Clone,
) -> Result<Option<Vec<SmolRoute>>, SystemError> {
    if !candidates
        .clone()
        .any(|(route, _)| route.expires_at_us().is_some())
    {
        return Ok(None);
    }
    let router_defaults = candidates
        .clone()
        .any(|(route, _)| route.learned_default().is_some());
    let mut ordered = Vec::new();
    ordered
        .try_reserve_exact(candidates.clone().count())
        .map_err(|_| SystemError::ENOMEM)?;
    ordered.extend(candidates);
    // smoltcp selects the last route among equally specific prefixes.
    // Retain alternatives so it can fall back even before physical cleanup.
    ordered.sort_unstable_by_key(|&(route, order)| {
        let (priority, preference, alias) = if router_defaults {
            route_order(route, order)
        } else {
            (0, RouterPreference::Medium, order)
        };
        core::cmp::Reverse((
            u8::from(route.table != RT_TABLE_LOCAL),
            priority,
            preference,
            alias,
        ))
    });
    let mut projection = Vec::new();
    projection
        .try_reserve_exact(ordered.len())
        .map_err(|_| SystemError::ENOMEM)?;
    projection.extend(ordered.into_iter().map(|(route, _)| smol_route(route)));
    Ok(Some(projection))
}

fn projection_order(
    route_index: usize,
    routes: &[RouteEntry],
    alias_order: &[AliasOrder],
) -> (u8, AliasOrder) {
    (
        u8::from(routes[route_index].table != RT_TABLE_LOCAL),
        alias_order[route_index],
    )
}

fn projection_route_precedes(
    candidate: usize,
    current: usize,
    routes: &[RouteEntry],
    alias_order: &[AliasOrder],
) -> bool {
    projection_order(candidate, routes, alias_order)
        < projection_order(current, routes, alias_order)
}

fn reserve_prefix_domain(
    counts: &mut HashMap<PrefixDomain, Vec<PrefixLengthCount>>,
    route: RouteEntry,
) -> Result<(), SystemError> {
    let domain = PrefixDomain {
        table: route.table,
        family: AddressFamily::of(route.destination.address()),
    };
    let prefix_len = route.destination.prefix_len();
    if let Some(lengths) = counts.get_mut(&domain) {
        if !lengths.iter().any(|entry| entry.prefix_len == prefix_len) {
            lengths.try_reserve(1).map_err(|_| SystemError::ENOMEM)?;
        }
    } else {
        counts.try_reserve(1).map_err(|_| SystemError::ENOMEM)?;
        let mut lengths = Vec::new();
        lengths
            .try_reserve_exact(1)
            .map_err(|_| SystemError::ENOMEM)?;
        counts.insert(domain, lengths);
    }
    Ok(())
}

fn update_prefix_count(
    counts: &mut HashMap<PrefixDomain, Vec<PrefixLengthCount>>,
    route: RouteEntry,
    present: bool,
) {
    let domain = PrefixDomain {
        table: route.table,
        family: AddressFamily::of(route.destination.address()),
    };
    let prefix_len = route.destination.prefix_len();
    let lengths = counts
        .get_mut(&domain)
        .expect("prefix-count domain must be reserved before commit");
    if present {
        if let Some(entry) = lengths
            .iter_mut()
            .find(|entry| entry.prefix_len == prefix_len)
        {
            entry.count = entry
                .count
                .checked_add(1)
                .expect("route count cannot overflow usize");
        } else {
            let position = lengths
                .iter()
                .position(|entry| entry.prefix_len < prefix_len)
                .unwrap_or(lengths.len());
            lengths.insert(
                position,
                PrefixLengthCount {
                    prefix_len,
                    count: 1,
                },
            );
        }
    } else {
        let position = lengths
            .iter()
            .position(|entry| entry.prefix_len == prefix_len)
            .expect("indexed route must have a prefix-length count");
        lengths[position].count = lengths[position]
            .count
            .checked_sub(1)
            .expect("route count cannot underflow");
        if lengths[position].count == 0 {
            lengths.remove(position);
        }
    }
}

fn remove_empty_prefix_domain(
    counts: &mut HashMap<PrefixDomain, Vec<PrefixLengthCount>>,
    route: RouteEntry,
) {
    let domain = PrefixDomain {
        table: route.table,
        family: AddressFamily::of(route.destination.address()),
    };
    if counts.get(&domain).is_some_and(Vec::is_empty) {
        counts.remove(&domain);
    }
}

pub(super) fn projection_key(route: RouteEntry) -> Option<ProjectionKey> {
    let projectable = route.source.is_none()
        && (route.table == super::RT_TABLE_MAIN && route.kind == RTN_UNICAST
            || route.table == super::RT_TABLE_LOCAL && route.kind == RTN_LOCAL);
    projectable.then(|| ProjectionKey::new(route.oif, route.destination))
}

fn ipv6_special(route: RouteEntry) -> bool {
    matches!(route.destination, IpCidr::Ipv6(_))
        && (route.kind == super::RTN_ANYCAST
            || super::is_ipv6_reject_or_throw(route.kind)
            || route.is_explicit_ipv6_local())
}

fn indexable(route: RouteEntry) -> bool {
    if let Some(source) = route.source {
        return matches!(
            (route.destination, source),
            (IpCidr::Ipv6(_), IpCidr::Ipv6(_))
        ) && source.prefix_len() != 0
            && route.tos == 0
            && (route.kind == RTN_UNICAST || ipv6_special(route));
    }
    route.tos == 0
        && (matches!(route.kind, RTN_UNICAST | RTN_LOCAL | RTN_BROADCAST)
            || matches!(route.destination, IpCidr::Ipv6(_)) && route.kind == RTN_MULTICAST
            || ipv6_special(route))
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use smoltcp::wire::{IpAddress, IpCidr, Ipv4Address, Ipv6Address};

    use super::{AliasPlacement, BroadcastLookup, FibIndex, FibLookupKey};
    use crate::net::route::{
        LearnedDefaultRoute, RouteEntry, RouterPreference, RTN_BROADCAST, RTN_UNICAST,
        RT_SCOPE_HOST, RT_SCOPE_LINK, RT_TABLE_MAIN,
    };

    fn route(address: [u8; 4], prefix_len: u8, priority: u32, kind: u8) -> RouteEntry {
        RouteEntry {
            destination: IpCidr::new(
                IpAddress::Ipv4(Ipv4Address::new(
                    address[0], address[1], address[2], address[3],
                )),
                prefix_len,
            ),
            source: None,
            preferred_source: None,
            table: RT_TABLE_MAIN,
            priority,
            tos: 0,
            protocol: 0,
            scope: 0,
            kind,
            oif: 1,
            gateway: None,
            nexthop_flags: 0,
            ipv6_address_local: false,
            ra_route: None,
        }
    }

    #[test]
    fn lookup_orders_prefix_metric_and_insertion() {
        let routes = [
            route([0, 0, 0, 0], 0, 0, RTN_UNICAST),
            route([192, 0, 2, 0], 24, 20, RTN_UNICAST),
            route([192, 0, 2, 0], 24, 10, RTN_UNICAST),
            route([192, 0, 2, 0], 24, 10, RTN_UNICAST),
        ];
        let index = FibIndex::build(&routes).unwrap();
        let destination = IpAddress::Ipv4(Ipv4Address::new(192, 0, 2, 9));

        assert_eq!(
            index.lookup(
                &routes,
                destination,
                RT_TABLE_MAIN,
                None,
                None,
                BroadcastLookup::Exclude,
            ),
            Some(2)
        );
    }

    #[test]
    fn ipv6_source_lookup_backtracks_at_the_matching_level() {
        let destination = Ipv6Address::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1).into();
        let network = Ipv6Address::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 0).into();
        let source = Ipv6Address::new(0xfd29, 0, 0, 0, 0, 0, 0, 1);
        let ordinary = RouteEntry {
            destination: IpCidr::new(network, 64),
            ..route([0; 4], 0, 10, RTN_UNICAST)
        };
        let parent = RouteEntry {
            destination: IpCidr::new(network, 32),
            ..ordinary
        };
        let specific = RouteEntry {
            source: Some(IpCidr::new(source.into(), 128)),
            oif: 2,
            priority: 200,
            ..ordinary
        };
        let broad = RouteEntry {
            source: Some(IpCidr::new(
                Ipv6Address::new(0xfd29, 0, 0, 0, 0, 0, 0, 0).into(),
                64,
            )),
            priority: 100,
            ..ordinary
        };
        let routes = [parent, ordinary, specific, broad];
        let index = FibIndex::build(&routes).unwrap();
        let lookup = |source, required_oif| {
            index.lookup_key(
                &routes,
                FibLookupKey {
                    destination,
                    source,
                    table: RT_TABLE_MAIN,
                    required_oif,
                    minimum_scope: None,
                    broadcast: BroadcastLookup::Exclude,
                },
            )
        };
        assert_eq!(lookup(None, None), Some(0));
        assert_eq!(lookup(Some(source), None), Some(2));
        assert_eq!(lookup(Some(source), Some(1)), Some(3));
    }

    #[test]
    fn learned_default_projection_falls_back_at_expiry() {
        let static_gateway = Ipv6Address::new(0xfe80, 0, 0, 0, 0, 0, 0, 1).into();
        let learned_gateway = Ipv6Address::new(0xfe80, 0, 0, 0, 0, 0, 0, 2).into();
        let default = RouteEntry {
            destination: IpCidr::new(Ipv6Address::UNSPECIFIED.into(), 0),
            gateway: Some(static_gateway),
            ..route([0; 4], 0, 1024, RTN_UNICAST)
        };
        let learned = RouteEntry {
            gateway: Some(learned_gateway),
            ipv6_address_local: false,
            ra_route: Some(crate::net::route::RaRoute::Default(LearnedDefaultRoute {
                expires_at_us: 10,
                preference: RouterPreference::High,
            })),
            ..default
        };
        let entries = [default, learned];
        let mut fib = FibIndex::build(&entries).unwrap();
        let projection = fib.projection_for_iface(&entries, default.oif).unwrap();
        let destination = Ipv6Address::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1).into();
        let mut smoltcp = smoltcp::iface::Routes::new();
        smoltcp.update(|routes| *routes = projection);
        assert_eq!(
            smoltcp.lookup(&destination, smoltcp::time::Instant::from_micros(9)),
            Some(learned_gateway)
        );
        assert_eq!(
            smoltcp.lookup(&destination, smoltcp::time::Instant::from_micros(10)),
            Some(static_gateway)
        );

        let mut expired_entries = entries;
        let mut default = learned.learned_default().unwrap();
        default.expires_at_us = 0;
        expired_entries[1].ra_route = Some(crate::net::route::RaRoute::Default(default));
        assert_eq!(
            fib.lookup(
                &expired_entries,
                destination,
                RT_TABLE_MAIN,
                None,
                None,
                BroadcastLookup::Exclude,
            ),
            Some(0)
        );

        let mut refreshed = entries;
        default.expires_at_us = i64::MAX;
        refreshed[1].ra_route = Some(crate::net::route::RaRoute::Default(default));
        fib.prepare_insert(refreshed[1]).unwrap();
        fib.commit_replace(1, learned, refreshed[1], &refreshed);
        let selected = |fib: &FibIndex, entries: &[RouteEntry]| {
            fib.lookup(
                entries,
                destination,
                RT_TABLE_MAIN,
                None,
                None,
                BroadcastLookup::Exclude,
            )
        };
        assert_eq!(selected(&fib, &refreshed), Some(1));
        let old = refreshed[1];
        default.preference = RouterPreference::Low;
        refreshed[1].ra_route = Some(crate::net::route::RaRoute::Default(default));
        fib.prepare_insert(refreshed[1]).unwrap();
        fib.commit_replace(1, old, refreshed[1], &refreshed);
        assert_eq!(selected(&fib, &refreshed), Some(0));
    }

    #[test]
    fn on_link_prefix_falls_back_before_cleanup() {
        let prefix = Ipv6Address::new(0xfd42, 0x2324, 0x47, 1, 0, 0, 0, 0);
        let destination = Ipv6Address::new(0xfd42, 0x2324, 0x47, 1, 0, 0, 0, 2).into();
        let gateway = Ipv6Address::new(0xfe80, 0, 0, 0, 0, 0, 0, 1).into();
        let fallback = RouteEntry {
            destination: IpCidr::new(prefix.into(), 64),
            gateway: Some(gateway),
            ..route([0; 4], 0, 512, RTN_UNICAST)
        };
        let learned = RouteEntry {
            priority: 256,
            gateway: None,
            ipv6_address_local: false,
            ra_route: Some(crate::net::route::RaRoute::OnLinkPrefix {
                expires_at_us: Some(10),
            }),
            ..fallback
        };
        let mut entries = [fallback, learned];
        let fib = FibIndex::build(&entries).unwrap();
        let projection = fib.projection_for_iface(&entries, fallback.oif).unwrap();
        let mut smoltcp = smoltcp::iface::Routes::new();
        smoltcp.update(|routes| *routes = projection);
        assert_eq!(
            smoltcp.lookup(&destination, smoltcp::time::Instant::from_micros(9)),
            Some(destination)
        );
        assert_eq!(
            smoltcp.lookup(&destination, smoltcp::time::Instant::from_micros(10)),
            Some(gateway)
        );

        // Leave the stale entry published so lookup must resolve its alias.
        entries[1].ra_route = Some(crate::net::route::RaRoute::OnLinkPrefix {
            expires_at_us: Some(i64::MIN),
        });
        assert_eq!(
            fib.lookup(
                &entries,
                destination,
                RT_TABLE_MAIN,
                None,
                None,
                BroadcastLookup::Exclude
            ),
            Some(0)
        );
    }

    #[test]
    fn incremental_insert_and_remove_match_rebuild() {
        let mut routes = vec![
            route([0, 0, 0, 0], 0, 0, RTN_UNICAST),
            route([192, 0, 2, 0], 24, 20, RTN_UNICAST),
        ];
        let mut index = FibIndex::build(&routes).unwrap();
        let added = route([192, 0, 2, 0], 24, 10, RTN_UNICAST);
        let placement = index.analyze_insert(added).placement;
        index.prepare_insert(added).unwrap();
        routes.push(added);
        index.commit_insert(2, added, placement, &routes);
        let destination = IpAddress::Ipv4(Ipv4Address::new(192, 0, 2, 9));
        assert_eq!(
            index.lookup(
                &routes,
                destination,
                RT_TABLE_MAIN,
                None,
                None,
                BroadcastLookup::Exclude,
            ),
            Some(2)
        );

        let removed = routes.swap_remove(2);
        index.commit_remove(2, removed, None, &routes);
        let rebuilt = FibIndex::build(&routes).unwrap();
        // An incremental index keeps advancing its alias group allocator.
        // Compare live ordering and derived caches with the fresh build.
        assert_eq!(index.all, rebuilt.all);
        assert_eq!(index.winners, rebuilt.winners);
        assert_eq!(index.projection, rebuilt.projection);
        assert_eq!(index.projection_winners, rebuilt.projection_winners);
        assert_eq!(index.prefix_counts, rebuilt.prefix_counts);
        for (slot, route) in routes.into_iter().enumerate() {
            assert_eq!(index.order_key(slot), rebuilt.order_key(slot));
            assert_eq!(index.analyze_insert(route).exact, Some(slot));
        }
    }

    #[test]
    fn incremental_insert_winners_match_full_rebuild() {
        let mut routes = Vec::new();
        let mut index = FibIndex::default();

        for ordinal in 0..64 {
            let mut added = route(
                [192, 0, 2, 0],
                24,
                (63 - ordinal) % 11,
                if ordinal % 5 == 0 {
                    RTN_BROADCAST
                } else {
                    RTN_UNICAST
                },
            );
            added.oif = ordinal % 4 + 1;
            added.scope = if ordinal % 3 == 0 {
                RT_SCOPE_HOST
            } else {
                RT_SCOPE_LINK
            };

            let placement = index.analyze_insert(added).placement;
            index.prepare_insert(added).unwrap();
            routes.push(added);
            index.commit_insert(routes.len() - 1, added, placement, &routes);

            let mut rebuilt = index.try_clone().unwrap();
            rebuilt.rebuild_all_winners(&routes);
            rebuilt.rebuild_all_projection_winners(&routes);
            assert_eq!(index, rebuilt);
        }
    }

    #[test]
    fn bulk_alias_insert_keeps_constant_time_metadata_and_semantic_order() {
        let mut routes: Vec<RouteEntry> = Vec::new();
        let mut semantic_order: Vec<usize> = Vec::new();
        let mut index = FibIndex::default();

        for ordinal in 0..512_u32 {
            let mut added = route([192, 0, 2, 0], 24, ordinal % 31, RTN_UNICAST);
            added.protocol = (ordinal % 255) as u8;
            added.gateway = Some(IpAddress::Ipv4(Ipv4Address::new(
                198,
                51,
                (ordinal / 256) as u8,
                (ordinal % 256) as u8,
            )));
            let analysis = index.analyze_insert(added);
            let expected_position = semantic_order
                .iter()
                .position(|candidate| routes[*candidate].priority >= added.priority)
                .unwrap_or(semantic_order.len());
            let placement = analysis.placement;

            index.prepare_insert(added).unwrap();
            let route_index = routes.len();
            routes.push(added);
            index.commit_insert(route_index, added, placement, &routes);
            // The physical bucket must only append. Reintroducing Vec::insert
            // here makes constructing one prefix deterministically quadratic.
            assert_eq!(index.prefix_candidates(added).last(), Some(&route_index));
            semantic_order.insert(expected_position, route_index);
        }

        let mut indexed_order: Vec<_> = (0..routes.len()).collect();
        indexed_order.sort_unstable_by_key(|route_index| index.order_key(*route_index));
        assert_eq!(indexed_order, semantic_order);
        assert_eq!(
            index.projection_winner(super::ProjectionKey::new(1, routes[0].destination)),
            semantic_order.first().copied()
        );
        assert!(routes
            .iter()
            .all(|route| index.analyze_insert(*route).exact.is_some()));
        assert!(matches!(
            index.analyze_insert(routes[0]).placement,
            AliasPlacement::Prepend
        ));
    }

    #[test]
    fn replacing_singleton_keeps_shared_index_buckets() {
        let old = route([192, 0, 2, 0], 24, 10, RTN_UNICAST);
        let mut new = old;
        new.oif = 2;
        let mut routes = vec![old];
        let mut index = FibIndex::build(&routes).unwrap();

        index.prepare_insert(new).unwrap();
        routes[0] = new;
        index.commit_replace(0, old, new, &routes);

        assert_eq!(index, FibIndex::build(&routes).unwrap());
    }

    #[test]
    fn swap_remove_preserves_unrelated_prefix_alias_order() {
        let unrelated = route([198, 51, 100, 0], 24, 0, RTN_UNICAST);
        let first = route([192, 0, 2, 0], 24, 10, RTN_UNICAST);
        let second = first;
        let mut routes = vec![unrelated, first, second];
        let mut index = FibIndex::build(&routes).unwrap();

        routes.swap_remove(0);
        index.commit_remove(0, unrelated, Some((2, second)), &routes);

        let destination = IpAddress::Ipv4(Ipv4Address::new(192, 0, 2, 9));
        assert_eq!(
            index.lookup(
                &routes,
                destination,
                RT_TABLE_MAIN,
                None,
                None,
                BroadcastLookup::Exclude,
            ),
            Some(1)
        );
    }

    #[test]
    fn bulk_retain_preserves_alias_order_after_slot_moves() {
        let unrelated = route([198, 51, 100, 0], 24, 0, RTN_UNICAST);
        let mut first = route([192, 0, 2, 0], 24, 10, RTN_UNICAST);
        first.protocol = 1;
        let mut removed_alias = first;
        removed_alias.protocol = 2;
        let mut last_alias = first;
        last_alias.protocol = 3;
        let mut routes = vec![unrelated, first, removed_alias, last_alias];
        let mut index = FibIndex::build(&routes).unwrap();

        routes.swap_remove(0);
        index.commit_remove(0, unrelated, Some((3, last_alias)), &routes);
        routes.retain(|route| *route != removed_alias);
        index.commit_retain(&[Some(0), Some(1), None], &routes);

        let destination = IpAddress::Ipv4(Ipv4Address::new(192, 0, 2, 9));
        assert_eq!(
            index.lookup(
                &routes,
                destination,
                RT_TABLE_MAIN,
                None,
                None,
                BroadcastLookup::Exclude,
            ),
            Some(1)
        );
        assert!(index.order_key(0) > index.order_key(1));
    }

    #[test]
    fn standard_lookup_does_not_scan_broadcast_candidates() {
        let routes = [
            route([192, 0, 2, 0], 24, 0, RTN_BROADCAST),
            route([0, 0, 0, 0], 0, 0, RTN_UNICAST),
        ];
        let index = FibIndex::build(&routes).unwrap();
        let destination = IpAddress::Ipv4(Ipv4Address::new(192, 0, 2, 9));

        assert_eq!(
            index.lookup(
                &routes,
                destination,
                RT_TABLE_MAIN,
                None,
                None,
                BroadcastLookup::Exclude,
            ),
            Some(1)
        );
        assert_eq!(
            index.lookup(
                &routes,
                destination,
                RT_TABLE_MAIN,
                None,
                None,
                BroadcastLookup::Any,
            ),
            Some(0)
        );
    }

    #[test]
    fn lookup_skips_absent_exact_prefix_bucket() {
        let routes = [
            route([192, 0, 2, 0], 24, 0, RTN_UNICAST),
            route([0, 0, 0, 0], 0, 0, RTN_UNICAST),
        ];
        let index = FibIndex::build(&routes).unwrap();
        let destination = IpAddress::Ipv4(Ipv4Address::new(198, 51, 100, 9));

        assert_eq!(
            index.lookup(
                &routes,
                destination,
                RT_TABLE_MAIN,
                None,
                None,
                BroadcastLookup::Exclude,
            ),
            Some(1)
        );
    }

    #[test]
    fn preselected_winners_cover_oif_scope_and_broadcast_selectors() {
        let mut any_oif = route([192, 0, 2, 0], 24, 1, RTN_UNICAST);
        any_oif.oif = 1;
        let mut link_scope = route([192, 0, 2, 0], 24, 5, RTN_UNICAST);
        link_scope.oif = 2;
        link_scope.scope = RT_SCOPE_LINK;
        let mut host_scope = route([192, 0, 2, 0], 24, 10, RTN_UNICAST);
        host_scope.oif = 2;
        host_scope.scope = RT_SCOPE_HOST;
        let mut broadcast = route([192, 0, 2, 0], 24, 0, RTN_BROADCAST);
        broadcast.oif = 3;
        let routes = [any_oif, link_scope, host_scope, broadcast];
        let index = FibIndex::build(&routes).unwrap();
        let destination = IpAddress::Ipv4(Ipv4Address::new(192, 0, 2, 9));

        assert_eq!(
            index.lookup(
                &routes,
                destination,
                RT_TABLE_MAIN,
                None,
                None,
                BroadcastLookup::Exclude,
            ),
            Some(0)
        );
        assert_eq!(
            index.lookup(
                &routes,
                destination,
                RT_TABLE_MAIN,
                Some(2),
                Some(RT_SCOPE_LINK),
                BroadcastLookup::Exclude,
            ),
            Some(1)
        );
        assert_eq!(
            index.lookup(
                &routes,
                destination,
                RT_TABLE_MAIN,
                Some(2),
                Some(RT_SCOPE_HOST),
                BroadcastLookup::Exclude,
            ),
            Some(2)
        );
        assert_eq!(
            index.lookup(
                &routes,
                destination,
                RT_TABLE_MAIN,
                Some(3),
                None,
                BroadcastLookup::OnIface(3),
            ),
            Some(3)
        );
        assert_eq!(
            index.lookup(
                &routes,
                destination,
                RT_TABLE_MAIN,
                None,
                None,
                BroadcastLookup::Any,
            ),
            Some(3)
        );
    }
}
