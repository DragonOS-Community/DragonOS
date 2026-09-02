use alloc::vec::Vec;

use smoltcp::wire::IpAddress;
use system_error::SystemError;

use super::{canonical_cidr, RouteEntry, RTN_BROADCAST, RTN_LOCAL, RTN_UNICAST};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct PrefixIndexEntry {
    table: u32,
    family: AddressFamily,
    // Reverse order makes the derived sort visit longest prefixes first.
    prefix_order: u8,
    destination: IpAddress,
    priority: u32,
    route_index: usize,
}

/// Derived longest-prefix-match index for the authoritative route vector.
///
/// The route vector remains the single source of truth and preserves RTNL
/// insertion/deletion order. This compact, allocation-fallible projection is
/// rebuilt on a transaction candidate and published together with that vector,
/// so readers can never observe mismatched route and index generations.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct FibIndex {
    standard: PrefixIndex,
    broadcast: PrefixIndex,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct PrefixIndex {
    entries: Vec<PrefixIndexEntry>,
}

impl FibIndex {
    pub(super) fn build(routes: &[RouteEntry]) -> Result<Self, SystemError> {
        let standard_count = routes
            .iter()
            .filter(|route| indexable(route) && route.kind != RTN_BROADCAST)
            .count();
        let broadcast_count = routes
            .iter()
            .filter(|route| indexable(route) && route.kind == RTN_BROADCAST)
            .count();
        let mut standard = PrefixIndex::with_capacity(standard_count)?;
        let mut broadcast = PrefixIndex::with_capacity(broadcast_count)?;
        for (route_index, route) in routes.iter().enumerate() {
            if !indexable(route) {
                continue;
            }
            let indexed = PrefixIndexEntry {
                table: route.table,
                family: AddressFamily::of(route.destination.address()),
                prefix_order: u8::MAX - route.destination.prefix_len(),
                destination: canonical_cidr(route.destination).address(),
                priority: route.priority,
                route_index,
            };
            if route.kind == RTN_BROADCAST {
                broadcast.entries.push(indexed);
            } else {
                standard.entries.push(indexed);
            }
        }
        // Within an exact prefix, Linux route preference is lower metric and
        // then earlier insertion. `route_index` encodes that final tie-break.
        standard.entries.sort_unstable();
        broadcast.entries.sort_unstable();
        Ok(Self {
            standard,
            broadcast,
        })
    }

    pub(super) fn try_clone(&self) -> Result<Self, SystemError> {
        Ok(Self {
            standard: self.standard.try_clone()?,
            broadcast: self.broadcast.try_clone()?,
        })
    }

    /// Visits only exact-prefix candidates, from longest to shortest.
    /// Candidates within a prefix are already ordered by metric/insertion.
    pub(super) fn lookup(
        &self,
        routes: &[RouteEntry],
        destination: IpAddress,
        table: u32,
        include_broadcast: bool,
        mut matches: impl FnMut(usize, &RouteEntry) -> bool,
    ) -> Option<usize> {
        let standard = self
            .standard
            .lookup(routes, destination, table, &mut matches);
        let broadcast = include_broadcast
            .then(|| {
                self.broadcast
                    .lookup(routes, destination, table, &mut matches)
            })
            .flatten();
        match (standard, broadcast) {
            (Some(left), Some(right)) => Some(preferred_route(routes, left, right)),
            (Some(route), None) | (None, Some(route)) => Some(route),
            (None, None) => None,
        }
    }
}

impl PrefixIndex {
    fn with_capacity(capacity: usize) -> Result<Self, SystemError> {
        let mut entries = Vec::new();
        entries
            .try_reserve_exact(capacity)
            .map_err(|_| SystemError::ENOMEM)?;
        Ok(Self { entries })
    }

    fn try_clone(&self) -> Result<Self, SystemError> {
        let mut cloned = Self::with_capacity(self.entries.len())?;
        cloned.entries.extend_from_slice(&self.entries);
        Ok(cloned)
    }

    fn lookup(
        &self,
        routes: &[RouteEntry],
        destination: IpAddress,
        table: u32,
        matches: &mut impl FnMut(usize, &RouteEntry) -> bool,
    ) -> Option<usize> {
        let family = AddressFamily::of(destination);
        let first = self
            .entries
            .partition_point(|entry| (entry.table, entry.family) < (table, family));
        let last = self
            .entries
            .partition_point(|entry| (entry.table, entry.family) <= (table, family));
        let mut family_entries = &self.entries[first..last];
        while let Some(first_entry) = family_entries.first() {
            let prefix_order = first_entry.prefix_order;
            let prefix_len = u8::MAX - prefix_order;
            let prefix =
                canonical_cidr(smoltcp::wire::IpCidr::new(destination, prefix_len)).address();
            let prefix_end =
                family_entries.partition_point(|entry| entry.prefix_order == prefix_order);
            let prefix_entries = &family_entries[..prefix_end];
            let candidate_start =
                prefix_entries.partition_point(|entry| entry.destination < prefix);
            for indexed in prefix_entries[candidate_start..]
                .iter()
                .take_while(|entry| entry.destination == prefix)
            {
                if matches(indexed.route_index, &routes[indexed.route_index]) {
                    return Some(indexed.route_index);
                }
            }
            family_entries = &family_entries[prefix_end..];
        }
        None
    }
}

fn indexable(route: &RouteEntry) -> bool {
    route.source.is_none()
        && route.tos == 0
        && matches!(route.kind, RTN_UNICAST | RTN_LOCAL | RTN_BROADCAST)
}

fn preferred_route(routes: &[RouteEntry], left: usize, right: usize) -> usize {
    let left_route = routes[left];
    let right_route = routes[right];
    let left_is_preferred = left_route
        .destination
        .prefix_len()
        .cmp(&right_route.destination.prefix_len())
        .then_with(|| right_route.priority.cmp(&left_route.priority))
        .then_with(|| right.cmp(&left))
        .is_ge();
    if left_is_preferred {
        left
    } else {
        right
    }
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
