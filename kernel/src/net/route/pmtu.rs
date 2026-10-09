//! Namespace-owned, bounded route exceptions. Transport validation precedes
//! learning; this table never calls back into sockets, FIB or netdevices.

use alloc::{sync::Arc, vec::Vec};
use smoltcp::wire::IpAddress;

use super::OutputRouteDecision;
use crate::{
    process::namespace::net_namespace::NetNamespace,
    time::{Duration, Instant},
};

const MAX_EXCEPTIONS: usize = 256;
const LIFETIME: Duration = Duration::from_secs(600);
const IPV4_MIN_PMTU: usize = 552;

#[derive(Clone, Copy, Debug)]
pub(crate) struct PathMtu {
    pub(crate) interface: usize,
    pub(crate) path: usize,
    pub(crate) locked: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Key {
    source: IpAddress,
    destination: IpAddress,
    oif: u32,
    next_hop: IpAddress,
    table: u32,
    interface_mtu: usize,
}

impl Key {
    fn new(route: OutputRouteDecision, source: IpAddress, destination: IpAddress) -> Self {
        // IPv4 exceptions belong to the destination/nexthop, not a socket's
        // chosen source. IPv6 source-specific routing needs both addresses.
        let source = match source {
            IpAddress::Ipv4(_) => IpAddress::v4(0, 0, 0, 0),
            source => source,
        };
        Self {
            source,
            destination,
            oif: route.oif,
            next_hop: route.next_hop,
            table: route.table,
            interface_mtu: route.ip_mtu,
        }
    }
}

#[derive(Debug)]
struct Exception {
    key: Key,
    mtu: usize,
    locked: bool,
    expires: Instant,
}

#[derive(Debug, Default)]
pub(crate) struct PmtuCache {
    entries: Vec<Exception>,
    generation: u64,
}

impl PmtuCache {
    pub(crate) fn invalidate(&mut self) {
        self.entries.clear();
        self.generation = self.generation.wrapping_add(1);
    }

    pub(crate) fn generation(&mut self) -> u64 {
        self.purge_expired(Instant::now());
        self.generation
    }

    fn purge_expired(&mut self, now: Instant) {
        let old_len = self.entries.len();
        self.entries.retain(|item| item.expires > now);
        if self.entries.len() != old_len {
            self.generation = self.generation.wrapping_add(1);
        }
    }

    fn lookup(&mut self, key: Key, now: Instant) -> PathMtu {
        self.purge_expired(now);
        let entry = self
            .entries
            .iter()
            .find(|item| item.key == key && item.expires > now);
        PathMtu {
            interface: key.interface_mtu,
            path: entry.map_or(key.interface_mtu, |item| item.mtu.min(key.interface_mtu)),
            locked: entry.is_some_and(|item| item.locked),
        }
    }

    fn learn(&mut self, key: Key, advertised: usize, now: Instant) -> PathMtu {
        let old = self.lookup(key, now);
        if old.locked || advertised > old.path {
            return old;
        }
        let ipv6 = matches!(key.destination, IpAddress::Ipv6(_));
        if ipv6 && (advertised < 1280 || advertised >= old.path) {
            return old;
        }
        let locked = !ipv6 && advertised < IPV4_MIN_PMTU;
        let mtu = if locked {
            old.path.min(IPV4_MIN_PMTU)
        } else {
            advertised
        };
        if mtu == old.path && !locked {
            if let Some(entry) = self.entries.iter().find(|item| item.key == key) {
                if entry.expires > now + Duration::from_secs(300) {
                    return old;
                }
            } else {
                return old;
            }
        }
        let expires = now + LIFETIME;
        if let Some(entry) = self.entries.iter_mut().find(|item| item.key == key) {
            entry.mtu = mtu;
            entry.locked = locked;
            entry.expires = expires;
        } else {
            if self.entries.len() == MAX_EXCEPTIONS {
                let oldest = self
                    .entries
                    .iter()
                    .enumerate()
                    .min_by_key(|(_, item)| item.expires)
                    .map(|(index, _)| index)
                    .unwrap();
                self.entries.swap_remove(oldest);
                self.generation = self.generation.wrapping_add(1);
            }
            if self.entries.try_reserve(1).is_err() {
                return old;
            }
            self.entries.push(Exception {
                key,
                mtu,
                locked,
                expires,
            });
        }
        self.lookup(key, now)
    }
}

pub(crate) fn path_mtu(
    netns: &NetNamespace,
    route: OutputRouteDecision,
    source: IpAddress,
    destination: IpAddress,
) -> PathMtu {
    netns
        .router()
        .pmtu
        .lock()
        .lookup(Key::new(route, source, destination), Instant::now())
}

/// Called only after the quoted flow has matched a live transport socket.
pub(crate) fn learn(
    netns: &Arc<NetNamespace>,
    feedback: &crate::net::pmtu::PmtuFeedback,
    required_oif: Option<u32>,
) -> Option<PathMtu> {
    let (source, destination) = crate::net::pmtu::routed_quote_flow(netns, feedback);
    let router = netns.router();
    let routes = super::lock_output_routes(&router, netns.device_list());
    let route = routes.lookup(destination, required_oif)?;
    // An unrelated external source must not create a local route exception.
    if routes
        .lookup(feedback.source, None)
        .is_none_or(|source_route| source_route.kind != super::RTN_LOCAL)
    {
        return None;
    }
    let learned = router.pmtu.lock().learn(
        Key::new(route, source, destination),
        feedback.mtu as usize,
        Instant::now(),
    );
    Some(learned)
}

pub(crate) fn learn_on_route(
    router: &crate::net::routing::Router,
    route: OutputRouteDecision,
    source: IpAddress,
    destination: IpAddress,
    advertised: u32,
) -> PathMtu {
    router.pmtu.lock().learn(
        Key::new(route, source, destination),
        advertised as usize,
        Instant::now(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(destination: IpAddress) -> Key {
        Key {
            source: IpAddress::v4(0, 0, 0, 0),
            destination,
            oif: 2,
            next_hop: destination,
            table: 254,
            interface_mtu: 1500,
        }
    }

    #[test]
    fn expiry_restores_mtu_and_invalidates_transport_hints_without_refresh_on_read() {
        let mut cache = PmtuCache::default();
        let key = key(IpAddress::v4(192, 0, 2, 1));
        assert_eq!(cache.learn(key, 1000, Instant::ZERO).path, 1000);
        let generation = cache.generation;
        assert_eq!(cache.lookup(key, Instant::from_secs(599)).path, 1000);
        assert_eq!(cache.generation, generation);
        assert_eq!(cache.lookup(key, Instant::from_secs(600)).path, 1500);
        assert_ne!(cache.generation, generation);
        assert!(cache.entries.is_empty());
    }

    #[test]
    fn ipv4_low_and_zero_mtu_lock_the_exception() {
        for advertised in [0, 128, 551] {
            let mut cache = PmtuCache::default();
            let key = key(IpAddress::v4(192, 0, 2, 1));
            let learned = cache.learn(key, advertised, Instant::ZERO);
            assert_eq!(learned.path, 552);
            assert!(learned.locked);
            assert_eq!(cache.learn(key, 500, Instant::from_secs(1)).path, 552);
        }
    }

    #[test]
    fn ipv6_rejects_low_equal_and_upward_feedback_without_extending_expiry() {
        let mut cache = PmtuCache::default();
        let key = key(IpAddress::v6(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1));
        assert_eq!(cache.learn(key, 1279, Instant::ZERO).path, 1500);
        assert!(cache.entries.is_empty());
        assert_eq!(cache.learn(key, 1280, Instant::ZERO).path, 1280);
        assert_eq!(cache.learn(key, 1400, Instant::from_secs(400)).path, 1280);
        assert_eq!(cache.learn(key, 1280, Instant::from_secs(400)).path, 1280);
        assert_eq!(cache.lookup(key, Instant::from_secs(600)).path, 1500);
    }

    #[test]
    fn ipv4_equal_feedback_refreshes_only_in_second_half_lifetime() {
        let mut cache = PmtuCache::default();
        let key = key(IpAddress::v4(192, 0, 2, 1));
        cache.learn(key, 1000, Instant::ZERO);
        cache.learn(key, 1000, Instant::from_secs(299));
        assert_eq!(cache.entries[0].expires, Instant::from_secs(600));
        cache.learn(key, 1000, Instant::from_secs(301));
        assert_eq!(cache.entries[0].expires, Instant::from_secs(901));
    }

    #[test]
    fn capacity_eviction_is_bounded_and_invalidates_transport_hints() {
        let mut cache = PmtuCache::default();
        for index in 0..MAX_EXCEPTIONS {
            cache.learn(
                key(IpAddress::v4(192, 0, 2, index as u8)),
                1000,
                Instant::ZERO,
            );
        }
        let generation = cache.generation;
        cache.learn(
            key(IpAddress::v4(198, 51, 100, 1)),
            900,
            Instant::from_secs(1),
        );
        assert_eq!(cache.entries.len(), MAX_EXCEPTIONS);
        assert_ne!(cache.generation, generation);
        cache.invalidate();
        assert!(cache.entries.is_empty());
    }
}
