use alloc::vec::Vec;

use hashbrown::HashMap;
use smoltcp::iface::Route as SmolRoute;
use smoltcp::wire::{IpAddress, IpCidr};
use system_error::SystemError;

use super::{canonical_cidr, RouteEntry, RTN_BROADCAST, RTN_LOCAL, RTN_UNICAST, RT_TABLE_LOCAL};

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
/// Buckets contain authoritative vector positions in insertion order. A
/// normal and lifecycle edits therefore update only their exact
/// prefix/projection buckets. `prefix_rank` keeps semantic alias order
/// independent of the authoritative vector slots used by `swap_remove`.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct FibIndex {
    all: HashMap<PrefixKey, Vec<usize>>,
    standard: HashMap<PrefixKey, Vec<usize>>,
    broadcast: HashMap<PrefixKey, Vec<usize>>,
    projection: HashMap<ProjectionKey, Vec<usize>>,
    prefix_counts: HashMap<PrefixDomain, Vec<PrefixLengthCount>>,
    prefix_rank: Vec<usize>,
}

impl FibIndex {
    #[cfg(test)]
    pub(super) fn build(routes: &[RouteEntry]) -> Result<Self, SystemError> {
        let mut index = Self::default();
        for (route_index, route) in routes.iter().copied().enumerate() {
            index.prepare_insert(route)?;
            index.commit_insert(route_index, route, None);
        }
        Ok(index)
    }

    pub(super) fn try_clone(&self) -> Result<Self, SystemError> {
        Ok(Self {
            all: try_clone_buckets(&self.all)?,
            standard: try_clone_buckets(&self.standard)?,
            broadcast: try_clone_buckets(&self.broadcast)?,
            projection: try_clone_buckets(&self.projection)?,
            prefix_counts: try_clone_prefix_counts(&self.prefix_counts)?,
            prefix_rank: try_clone_vec(&self.prefix_rank)?,
        })
    }

    /// Reserve every bucket needed by a later infallible commit.
    pub(super) fn prepare_insert(&mut self, route: RouteEntry) -> Result<(), SystemError> {
        let result = (|| {
            self.prefix_rank
                .try_reserve(1)
                .map_err(|_| SystemError::ENOMEM)?;
            reserve_bucket(
                &mut self.all,
                PrefixKey::new(route.table, route.destination),
            )?;
            if indexable(route) {
                reserve_prefix_domain(&mut self.prefix_counts, route)?;
                let buckets = if route.kind == RTN_BROADCAST {
                    &mut self.broadcast
                } else {
                    &mut self.standard
                };
                reserve_bucket(buckets, PrefixKey::new(route.table, route.destination))?;
            }
            if let Some(key) = projection_key(route) {
                reserve_bucket(&mut self.projection, key)?;
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
            let buckets = if route.kind == RTN_BROADCAST {
                &mut self.broadcast
            } else {
                &mut self.standard
            };
            remove_empty_bucket(buckets, &PrefixKey::new(route.table, route.destination));
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
        before: Option<usize>,
    ) {
        let prefix = PrefixKey::new(route.table, route.destination);
        let rank = before
            .map(|before| self.prefix_rank[before])
            .unwrap_or_else(|| self.all.get(&prefix).map_or(0, Vec::len));
        if let Some(indices) = self.all.get(&prefix) {
            for index in indices {
                if self.prefix_rank[*index] >= rank {
                    self.prefix_rank[*index] += 1;
                }
            }
        }
        debug_assert_eq!(route_index, self.prefix_rank.len());
        self.prefix_rank.push(rank);
        insert_reserved_bucket(&mut self.all, prefix, route_index, before);
        if indexable(route) {
            let buckets = if route.kind == RTN_BROADCAST {
                &mut self.broadcast
            } else {
                &mut self.standard
            };
            insert_reserved_subset_bucket(buckets, prefix, route_index, &self.prefix_rank);
            update_prefix_count(&mut self.prefix_counts, route, true);
        }
        if let Some(key) = projection_key(route) {
            insert_reserved_bucket(&mut self.projection, key, route_index, None);
        }
    }

    pub(super) fn commit_remove(
        &mut self,
        route_index: usize,
        route: RouteEntry,
        moved: Option<(usize, RouteEntry)>,
    ) {
        let removed_rank = self.prefix_rank[route_index];
        remove_index(
            &mut self.all,
            &PrefixKey::new(route.table, route.destination),
            route_index,
        );
        if indexable(route) {
            let buckets = if route.kind == RTN_BROADCAST {
                &mut self.broadcast
            } else {
                &mut self.standard
            };
            remove_index(
                buckets,
                &PrefixKey::new(route.table, route.destination),
                route_index,
            );
        }
        if let Some(key) = projection_key(route) {
            remove_index(&mut self.projection, &key, route_index);
        }
        let prefix = PrefixKey::new(route.table, route.destination);
        if let Some(indices) = self.all.get(&prefix) {
            for index in indices {
                if self.prefix_rank[*index] > removed_rank {
                    self.prefix_rank[*index] -= 1;
                }
            }
        }
        self.prefix_rank.swap_remove(route_index);
        if indexable(route) {
            update_prefix_count(&mut self.prefix_counts, route, false);
            remove_empty_prefix_domain(&mut self.prefix_counts, route);
        }
        if let Some((old_index, moved_route)) = moved {
            self.replace_route_index(old_index, route_index, moved_route);
        }
    }

    /// Apply a stable compaction of the authoritative route vector.
    ///
    /// `old_to_new` maps every old vector slot to its retained slot, or to
    /// `None` when the route was removed. Bucket order is preserved, so exact
    /// prefix aliases keep their semantic insertion order even when earlier
    /// single-route removals have made physical vector order differ from it.
    pub(super) fn commit_retain(&mut self, old_to_new: &[Option<usize>], routes: &[RouteEntry]) {
        debug_assert_eq!(old_to_new.len(), self.prefix_rank.len());
        debug_assert_eq!(
            old_to_new.iter().filter(|index| index.is_some()).count(),
            routes.len()
        );

        remap_retained_indices(&mut self.all, old_to_new);
        remap_retained_indices(&mut self.standard, old_to_new);
        remap_retained_indices(&mut self.broadcast, old_to_new);
        remap_retained_indices(&mut self.projection, old_to_new);

        self.prefix_rank.truncate(routes.len());
        self.prefix_rank.fill(0);
        for indices in self.all.values() {
            for (rank, route_index) in indices.iter().copied().enumerate() {
                self.prefix_rank[route_index] = rank;
            }
        }

        for counts in self.prefix_counts.values_mut() {
            counts.clear();
        }
        for route in routes.iter().copied().filter(|route| indexable(*route)) {
            update_prefix_count(&mut self.prefix_counts, route, true);
        }
        self.prefix_counts.retain(|_, counts| !counts.is_empty());
    }

    pub(super) fn commit_replace(&mut self, route_index: usize, old: RouteEntry, new: RouteEntry) {
        let old_prefix = PrefixKey::new(old.table, old.destination);
        let new_prefix = PrefixKey::new(new.table, new.destination);
        debug_assert_eq!(old_prefix, new_prefix);

        let old_standard = indexable(old).then_some(old.kind != RTN_BROADCAST);
        let new_standard = indexable(new).then_some(new.kind != RTN_BROADCAST);
        if old_standard != new_standard {
            if let Some(standard) = old_standard {
                let buckets = if standard {
                    &mut self.standard
                } else {
                    &mut self.broadcast
                };
                remove_index(buckets, &old_prefix, route_index);
                update_prefix_count(&mut self.prefix_counts, old, false);
            }
            if let Some(standard) = new_standard {
                let buckets = if standard {
                    &mut self.standard
                } else {
                    &mut self.broadcast
                };
                insert_reserved_subset_bucket(buckets, new_prefix, route_index, &self.prefix_rank);
                update_prefix_count(&mut self.prefix_counts, new, true);
            }
            remove_empty_prefix_domain(&mut self.prefix_counts, old);
        }

        let old_projection = projection_key(old);
        let new_projection = projection_key(new);
        if old_projection != new_projection {
            if let Some(key) = old_projection {
                remove_index(&mut self.projection, &key, route_index);
            }
            if let Some(key) = new_projection {
                insert_reserved_bucket(&mut self.projection, key, route_index, None);
            }
        }
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

    pub(super) fn route_order(&self, route_index: usize, route: RouteEntry) -> usize {
        debug_assert!(self.prefix_candidates(route).contains(&route_index));
        self.prefix_rank[route_index]
    }

    pub(super) fn projection_for_iface(
        &self,
        routes: &[RouteEntry],
        ifindex: u32,
    ) -> Result<Vec<SmolRoute>, SystemError> {
        let count = self
            .projection
            .keys()
            .filter(|key| key.oif == ifindex)
            .count();
        let mut projection = Vec::new();
        projection
            .try_reserve_exact(count)
            .map_err(|_| SystemError::ENOMEM)?;
        for (key, candidates) in self.projection.iter().filter(|(key, _)| key.oif == ifindex) {
            let winner = candidates
                .iter()
                .copied()
                .min_by_key(|index| {
                    let route = routes[*index];
                    (
                        u8::from(route.table != RT_TABLE_LOCAL),
                        route.priority,
                        self.route_order(*index, route),
                    )
                })
                .expect("projection buckets cannot be empty");
            let route = routes[winner];
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
        include_broadcast: bool,
        mut matches: impl FnMut(usize, &RouteEntry) -> bool,
    ) -> Option<usize> {
        let domain = PrefixDomain {
            table,
            family: AddressFamily::of(destination),
        };
        let counts = self.prefix_counts.get(&domain)?;
        for prefix_len in counts.iter().map(|entry| entry.prefix_len) {
            let key = PrefixKey::for_lookup(table, destination, prefix_len);
            let standard = preferred_candidate(self.standard.get(&key), routes, &mut matches);
            let broadcast = include_broadcast
                .then(|| preferred_candidate(self.broadcast.get(&key), routes, &mut matches))
                .flatten();
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
            .then_with(|| {
                self.route_order(right, right_route)
                    .cmp(&self.route_order(left, left_route))
            })
            .is_ge();
        if left_is_preferred {
            left
        } else {
            right
        }
    }

    fn replace_route_index(&mut self, old_index: usize, new_index: usize, route: RouteEntry) {
        replace_index(
            &mut self.all,
            &PrefixKey::new(route.table, route.destination),
            old_index,
            new_index,
        );
        if indexable(route) {
            let buckets = if route.kind == RTN_BROADCAST {
                &mut self.broadcast
            } else {
                &mut self.standard
            };
            replace_index(
                buckets,
                &PrefixKey::new(route.table, route.destination),
                old_index,
                new_index,
            );
        }
        if let Some(key) = projection_key(route) {
            replace_index(&mut self.projection, &key, old_index, new_index);
        }
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

fn try_clone_vec(source: &[usize]) -> Result<Vec<usize>, SystemError> {
    let mut cloned = Vec::new();
    cloned
        .try_reserve_exact(source.len())
        .map_err(|_| SystemError::ENOMEM)?;
    cloned.extend_from_slice(source);
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

fn insert_reserved_subset_bucket<K>(
    buckets: &mut HashMap<K, Vec<usize>>,
    key: K,
    route_index: usize,
    prefix_rank: &[usize],
) where
    K: Copy + Eq + core::hash::Hash,
{
    let indices = buckets
        .get_mut(&key)
        .expect("route index bucket must be reserved before commit");
    let route_rank = prefix_rank[route_index];
    let position = indices
        .iter()
        .position(|candidate| prefix_rank[*candidate] > route_rank)
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

fn preferred_candidate(
    candidates: Option<&Vec<usize>>,
    routes: &[RouteEntry],
    matches: &mut impl FnMut(usize, &RouteEntry) -> bool,
) -> Option<usize> {
    candidates?
        .iter()
        .copied()
        .enumerate()
        .filter(|item| matches(item.1, &routes[item.1]))
        .min_by_key(|item| (routes[item.1].priority, item.0))
        .map(|item| item.1)
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
    use smoltcp::wire::{IpAddress, IpCidr, Ipv4Address};

    use super::FibIndex;
    use crate::net::route::{RouteEntry, RTN_BROADCAST, RTN_UNICAST, RT_TABLE_MAIN};

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
            index.lookup(&routes, destination, RT_TABLE_MAIN, false, |_, _| true),
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
        index.prepare_insert(added).unwrap();
        routes.push(added);
        index.commit_insert(2, added, Some(1));
        let destination = IpAddress::Ipv4(Ipv4Address::new(192, 0, 2, 9));
        assert_eq!(
            index.lookup(&routes, destination, RT_TABLE_MAIN, false, |_, _| true),
            Some(2)
        );

        let removed = routes.swap_remove(2);
        index.commit_remove(2, removed, None);
        assert_eq!(index, FibIndex::build(&routes).unwrap());
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
        index.commit_replace(0, old, new);

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
        index.commit_remove(0, unrelated, Some((2, second)));

        let destination = IpAddress::Ipv4(Ipv4Address::new(192, 0, 2, 9));
        assert_eq!(
            index.lookup(&routes, destination, RT_TABLE_MAIN, false, |_, _| true),
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
        index.commit_remove(0, unrelated, Some((3, last_alias)));
        routes.retain(|route| *route != removed_alias);
        index.commit_retain(&[Some(0), Some(1), None], &routes);

        let destination = IpAddress::Ipv4(Ipv4Address::new(192, 0, 2, 9));
        assert_eq!(
            index.lookup(&routes, destination, RT_TABLE_MAIN, false, |_, _| true),
            Some(1)
        );
        assert_eq!(index.prefix_rank, [1, 0]);
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
            index.lookup(&routes, destination, RT_TABLE_MAIN, false, |_, _| true),
            Some(1)
        );
        assert_eq!(
            index.lookup(&routes, destination, RT_TABLE_MAIN, true, |_, _| true),
            Some(0)
        );
    }
}
