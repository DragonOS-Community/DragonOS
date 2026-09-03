use alloc::vec::Vec;

use hashbrown::HashMap;
use smoltcp::iface::Route as SmolRoute;
use smoltcp::wire::{IpAddress, IpCidr};
use system_error::SystemError;

use super::{
    canonical_cidr, RouteEntry, RTN_BROADCAST, RTN_LOCAL, RTN_UNICAST, RT_SCOPE_HOST,
    RT_SCOPE_LINK, RT_TABLE_LOCAL,
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
struct PrefixWinners {
    standard: CandidateWinners,
    broadcast: CandidateWinners,
}

impl PrefixWinners {
    fn clear(&mut self) {
        self.standard.clear();
        self.broadcast.clear();
    }

    fn is_empty(&self) -> bool {
        self.standard.any_oif == ScopeWinners::default()
            && self.standard.by_oif.is_empty()
            && self.broadcast.any_oif == ScopeWinners::default()
            && self.broadcast.by_oif.is_empty()
    }

    fn references(&self, route_index: usize) -> bool {
        self.standard.references(route_index) || self.broadcast.references(route_index)
    }

    fn replace_index(&mut self, old_index: usize, new_index: usize) {
        self.standard.replace_index(old_index, new_index);
        self.broadcast.replace_index(old_index, new_index);
    }

    fn candidates_mut(&mut self, route: RouteEntry) -> &mut CandidateWinners {
        if route.kind == RTN_BROADCAST {
            &mut self.broadcast
        } else {
            &mut self.standard
        }
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
}

/// Derived lookup and projection indexes for the authoritative route vector.
///
/// `all` buckets contain authoritative vector positions in insertion order.
/// `winners` preselects every selector used by packet-path lookups, so an
/// exact-prefix lookup never scans an attacker-controlled alias list while
/// holding the FIB lock. Lifecycle edits rebuild only affected prefix winners.
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
        })
    }

    pub(super) fn has_ipv4_routes(&self, table: u32) -> bool {
        self.prefix_counts.contains_key(&PrefixDomain {
            table,
            family: AddressFamily::Ipv4,
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
            remove_empty_winner(
                &mut self.winners,
                &PrefixKey::new(route.table, route.destination),
            );
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
            update_prefix_count(&mut self.prefix_counts, route, true);
            // Existing winners remain valid across insertion: relative order
            // between existing aliases is unchanged, so only the new route
            // can displace them. Deletion/replacement still rebuilds when a
            // cached winner may disappear or change class.
            self.winners
                .get_mut(&prefix)
                .expect("winner prefix must be reserved before commit")
                .candidates_mut(route)
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
            .is_some_and(|winners| winners.references(route_index));
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

        remap_retained_indices(&mut self.all, old_to_new);
        remap_retained_indices(&mut self.projection, old_to_new);

        self.alias.commit_retain(old_to_new, routes);

        for counts in self.prefix_counts.values_mut() {
            counts.clear();
        }
        for route in routes.iter().copied().filter(|route| indexable(*route)) {
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
        let old_was_winner = self
            .winners
            .get(&old_prefix)
            .is_some_and(|winners| winners.references(route_index));
        if old_indexable != new_indexable {
            update_prefix_count(
                &mut self.prefix_counts,
                if old_indexable { old } else { new },
                new_indexable,
            );
            remove_empty_prefix_domain(&mut self.prefix_counts, old);
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
                .candidates_mut(new)
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
        self.all
            .get(&PrefixKey::new(route.table, route.destination))
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
        for (key, winner) in self
            .projection_winners
            .iter()
            .filter(|(key, _)| key.oif == ifindex)
        {
            let route = routes[*winner];
            projection.push(SmolRoute {
                cidr: key.cidr(),
                via_router: route.gateway,
                preferred_until: None,
                expires_at: None,
            });
        }
        projection.sort_unstable_by_key(|route| route.cidr);
        Ok(projection)
    }

    /// Visits exact-prefix buckets from longest to shortest. Work is bounded
    /// by the address-family prefix width rather than by route-table size.
    pub(super) fn lookup(
        &self,
        routes: &[RouteEntry],
        destination: IpAddress,
        table: u32,
        required_oif: Option<u32>,
        minimum_scope: Option<u8>,
        broadcast: BroadcastLookup,
    ) -> Option<usize> {
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
            let standard = winners.standard.get(required_oif, scope);
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
            winners.replace_index(old_index, new_index);
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
                    winners.candidates_mut(route).consider(
                        route_index,
                        route,
                        routes,
                        self.alias.orders(),
                    );
                }
            }
        }
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
                    winners.candidates_mut(route).consider(
                        route_index,
                        route,
                        routes,
                        self.alias.orders(),
                    );
                }
            }
        }
        self.winners.retain(|_, winners| !winners.is_empty());
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
            },
        );
    }
    Ok(cloned)
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
        .candidates_mut(route)
        .by_oif
        .try_reserve(1)
        .map_err(|_| SystemError::ENOMEM)
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
    _routes: &[RouteEntry],
    alias_order: &[AliasOrder],
) -> bool {
    alias_order[candidate] < alias_order[current]
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

fn indexable(route: RouteEntry) -> bool {
    route.source.is_none()
        && route.tos == 0
        && matches!(route.kind, RTN_UNICAST | RTN_LOCAL | RTN_BROADCAST)
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use smoltcp::wire::{IpAddress, IpCidr, Ipv4Address};

    use super::{AliasPlacement, BroadcastLookup, FibIndex};
    use crate::net::route::{
        RouteEntry, RTN_BROADCAST, RTN_UNICAST, RT_SCOPE_HOST, RT_SCOPE_LINK, RT_TABLE_MAIN,
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
        assert_eq!(index, FibIndex::build(&routes).unwrap());
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
        let mut routes = Vec::new();
        let mut semantic_order = Vec::new();
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
