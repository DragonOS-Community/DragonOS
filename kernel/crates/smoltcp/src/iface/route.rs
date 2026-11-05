#[cfg(feature = "alloc")]
use alloc::vec::Vec as AllocVec;
#[cfg(not(feature = "alloc"))]
use heapless::Vec as HeaplessVec;

#[cfg(not(feature = "alloc"))]
use crate::config::IFACE_MAX_ROUTE_COUNT;
use crate::time::Instant;
use crate::wire::{IpAddress, IpCidr};
#[cfg(feature = "proto-ipv4")]
use crate::wire::{Ipv4Address, Ipv4Cidr};
#[cfg(feature = "proto-ipv6")]
use crate::wire::{Ipv6Address, Ipv6Cidr};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub struct RouteTableFull;

#[cfg(feature = "alloc")]
pub type RouteStorage = AllocVec<Route>;
#[cfg(not(feature = "alloc"))]
pub type RouteStorage = HeaplessVec<Route, IFACE_MAX_ROUTE_COUNT>;

impl core::fmt::Display for RouteTableFull {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Route table full")
    }
}

#[cfg(feature = "std")]
impl std::error::Error for RouteTableFull {}

/// A prefix of addresses that should be routed either via a router or directly on-link.
#[derive(Debug, Clone, Copy)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub struct Route {
    pub cidr: IpCidr,
    /// `Some(router)` means route via a next-hop router. `None` means the
    /// destination is on-link and should be resolved directly.
    pub via_router: Option<IpAddress>,
    /// `None` means "forever".
    pub preferred_until: Option<Instant>,
    /// `None` means "forever".
    pub expires_at: Option<Instant>,
}

#[cfg(feature = "proto-ipv4")]
const IPV4_DEFAULT: IpCidr = IpCidr::Ipv4(Ipv4Cidr::new(Ipv4Address::new(0, 0, 0, 0), 0));
#[cfg(feature = "proto-ipv6")]
const IPV6_DEFAULT: IpCidr =
    IpCidr::Ipv6(Ipv6Cidr::new(Ipv6Address::new(0, 0, 0, 0, 0, 0, 0, 0), 0));

impl Route {
    /// Returns a route to 0.0.0.0/0 via the `gateway`, with no expiry.
    #[cfg(feature = "proto-ipv4")]
    pub fn new_ipv4_gateway(gateway: Ipv4Address) -> Route {
        Route {
            cidr: IPV4_DEFAULT,
            via_router: Some(gateway.into()),
            preferred_until: None,
            expires_at: None,
        }
    }

    /// Returns a route to ::/0 via the `gateway`, with no expiry.
    #[cfg(feature = "proto-ipv6")]
    pub fn new_ipv6_gateway(gateway: Ipv6Address) -> Route {
        Route {
            cidr: IPV6_DEFAULT,
            via_router: Some(gateway.into()),
            preferred_until: None,
            expires_at: None,
        }
    }
}

/// A routing table.
#[derive(Debug)]
pub struct Routes {
    storage: RouteStorage,
}

impl Routes {
    /// Creates a new empty routing table.
    pub fn new() -> Self {
        Self {
            storage: RouteStorage::new(),
        }
    }

    /// Update the routes of this node.
    pub fn update<F: FnOnce(&mut RouteStorage)>(&mut self, f: F) {
        f(&mut self.storage);
    }

    #[cfg(feature = "alloc")]
    fn push(&mut self, route: Route) -> Result<(), RouteTableFull> {
        self.storage.push(route);
        Ok(())
    }

    #[cfg(not(feature = "alloc"))]
    fn push(&mut self, route: Route) -> Result<(), RouteTableFull> {
        self.storage.push(route).map_err(|_| RouteTableFull)
    }

    /// Add a default ipv4 gateway (ie. "ip route add 0.0.0.0/0 via `gateway`").
    ///
    /// On success, returns the previous default route, if any.
    #[cfg(feature = "proto-ipv4")]
    pub fn add_default_ipv4_route(
        &mut self,
        gateway: Ipv4Address,
    ) -> Result<Option<Route>, RouteTableFull> {
        let old = self.remove_default_ipv4_route();
        self.push(Route::new_ipv4_gateway(gateway))?;
        Ok(old)
    }

    /// Add a default ipv6 gateway (ie. "ip -6 route add ::/0 via `gateway`").
    ///
    /// On success, returns the previous default route, if any.
    #[cfg(feature = "proto-ipv6")]
    pub fn add_default_ipv6_route(
        &mut self,
        gateway: Ipv6Address,
    ) -> Result<Option<Route>, RouteTableFull> {
        let old = self.remove_default_ipv6_route();
        self.push(Route::new_ipv6_gateway(gateway))?;
        Ok(old)
    }

    /// Remove the default ipv4 gateway
    ///
    /// On success, returns the previous default route, if any.
    #[cfg(feature = "proto-ipv4")]
    pub fn remove_default_ipv4_route(&mut self) -> Option<Route> {
        if let Some((i, _)) = self
            .storage
            .iter()
            .enumerate()
            .find(|(_, r)| r.cidr == IPV4_DEFAULT)
        {
            Some(self.storage.remove(i))
        } else {
            None
        }
    }

    /// Remove the default ipv6 gateway
    ///
    /// On success, returns the previous default route, if any.
    #[cfg(feature = "proto-ipv6")]
    pub fn remove_default_ipv6_route(&mut self) -> Option<Route> {
        if let Some((i, _)) = self
            .storage
            .iter()
            .enumerate()
            .find(|(_, r)| r.cidr == IPV6_DEFAULT)
        {
            Some(self.storage.remove(i))
        } else {
            None
        }
    }

    pub fn lookup(&self, addr: &IpAddress, timestamp: Instant) -> Option<IpAddress> {
        assert!(addr.is_unicast());

        self.storage
            .iter()
            // Keep only matching routes
            .filter(|route| {
                if let Some(expires_at) = route.expires_at {
                    if timestamp > expires_at {
                        return false;
                    }
                }
                route.cidr.contains_addr(addr)
            })
            // pick the most specific one (highest prefix_len)
            .max_by_key(|route| route.cidr.prefix_len())
            .map(|route| route.via_router.unwrap_or(*addr))
    }
}

#[cfg(test)]
mod test {
    use super::*;

    fn push_route(storage: &mut RouteStorage, route: Route) {
        #[cfg(feature = "alloc")]
        storage.push(route);
        #[cfg(not(feature = "alloc"))]
        storage.push(route).unwrap();
    }
    #[cfg(feature = "proto-ipv6")]
    mod mock {
        use super::super::*;
        pub const ADDR_1A: Ipv6Address = Ipv6Address::new(0xfe80, 0, 0, 2, 0, 0, 0, 1);
        pub const ADDR_1B: Ipv6Address = Ipv6Address::new(0xfe80, 0, 0, 2, 0, 0, 0, 13);
        pub const ADDR_1C: Ipv6Address = Ipv6Address::new(0xfe80, 0, 0, 2, 0, 0, 0, 42);
        pub fn cidr_1() -> Ipv6Cidr {
            Ipv6Cidr::new(Ipv6Address::new(0xfe80, 0, 0, 2, 0, 0, 0, 0), 64)
        }

        pub const ADDR_2A: Ipv6Address = Ipv6Address::new(0xfe80, 0, 0, 0x3364, 0, 0, 0, 1);
        pub const ADDR_2B: Ipv6Address = Ipv6Address::new(0xfe80, 0, 0, 0x3364, 0, 0, 0, 21);
        pub fn cidr_2() -> Ipv6Cidr {
            Ipv6Cidr::new(Ipv6Address::new(0xfe80, 0, 0, 0x3364, 0, 0, 0, 0), 64)
        }
    }

    #[cfg(all(feature = "proto-ipv4", not(feature = "proto-ipv6")))]
    mod mock {
        use super::super::*;
        pub const ADDR_1A: Ipv4Address = Ipv4Address::new(192, 0, 2, 1);
        pub const ADDR_1B: Ipv4Address = Ipv4Address::new(192, 0, 2, 13);
        pub const ADDR_1C: Ipv4Address = Ipv4Address::new(192, 0, 2, 42);
        pub fn cidr_1() -> Ipv4Cidr {
            Ipv4Cidr::new(Ipv4Address::new(192, 0, 2, 0), 24)
        }

        pub const ADDR_2A: Ipv4Address = Ipv4Address::new(198, 51, 100, 1);
        pub const ADDR_2B: Ipv4Address = Ipv4Address::new(198, 51, 100, 21);
        pub fn cidr_2() -> Ipv4Cidr {
            Ipv4Cidr::new(Ipv4Address::new(198, 51, 100, 0), 24)
        }
    }

    use self::mock::*;

    #[test]
    fn test_fill() {
        let mut routes = Routes::new();

        assert_eq!(
            routes.lookup(&ADDR_1A.into(), Instant::from_millis(0)),
            None
        );
        assert_eq!(
            routes.lookup(&ADDR_1B.into(), Instant::from_millis(0)),
            None
        );
        assert_eq!(
            routes.lookup(&ADDR_1C.into(), Instant::from_millis(0)),
            None
        );
        assert_eq!(
            routes.lookup(&ADDR_2A.into(), Instant::from_millis(0)),
            None
        );
        assert_eq!(
            routes.lookup(&ADDR_2B.into(), Instant::from_millis(0)),
            None
        );

        let route = Route {
            cidr: cidr_1().into(),
            via_router: Some(ADDR_1A.into()),
            preferred_until: None,
            expires_at: None,
        };
        routes.update(|storage| {
            push_route(storage, route);
        });

        assert_eq!(
            routes.lookup(&ADDR_1A.into(), Instant::from_millis(0)),
            Some(ADDR_1A.into())
        );
        assert_eq!(
            routes.lookup(&ADDR_1B.into(), Instant::from_millis(0)),
            Some(ADDR_1A.into())
        );
        assert_eq!(
            routes.lookup(&ADDR_1C.into(), Instant::from_millis(0)),
            Some(ADDR_1A.into())
        );
        assert_eq!(
            routes.lookup(&ADDR_2A.into(), Instant::from_millis(0)),
            None
        );
        assert_eq!(
            routes.lookup(&ADDR_2B.into(), Instant::from_millis(0)),
            None
        );

        let route2 = Route {
            cidr: cidr_2().into(),
            via_router: Some(ADDR_2A.into()),
            preferred_until: Some(Instant::from_millis(10)),
            expires_at: Some(Instant::from_millis(10)),
        };
        routes.update(|storage| {
            push_route(storage, route2);
        });

        assert_eq!(
            routes.lookup(&ADDR_1A.into(), Instant::from_millis(0)),
            Some(ADDR_1A.into())
        );
        assert_eq!(
            routes.lookup(&ADDR_1B.into(), Instant::from_millis(0)),
            Some(ADDR_1A.into())
        );
        assert_eq!(
            routes.lookup(&ADDR_1C.into(), Instant::from_millis(0)),
            Some(ADDR_1A.into())
        );
        assert_eq!(
            routes.lookup(&ADDR_2A.into(), Instant::from_millis(0)),
            Some(ADDR_2A.into())
        );
        assert_eq!(
            routes.lookup(&ADDR_2B.into(), Instant::from_millis(0)),
            Some(ADDR_2A.into())
        );

        assert_eq!(
            routes.lookup(&ADDR_1A.into(), Instant::from_millis(10)),
            Some(ADDR_1A.into())
        );
        assert_eq!(
            routes.lookup(&ADDR_1B.into(), Instant::from_millis(10)),
            Some(ADDR_1A.into())
        );
        assert_eq!(
            routes.lookup(&ADDR_1C.into(), Instant::from_millis(10)),
            Some(ADDR_1A.into())
        );
        assert_eq!(
            routes.lookup(&ADDR_2A.into(), Instant::from_millis(10)),
            Some(ADDR_2A.into())
        );
        assert_eq!(
            routes.lookup(&ADDR_2B.into(), Instant::from_millis(10)),
            Some(ADDR_2A.into())
        );
    }

    #[test]
    fn test_on_link_route_returns_destination() {
        let mut routes = Routes::new();
        routes.update(|storage| {
            push_route(
                storage,
                Route {
                    cidr: cidr_1().into(),
                    via_router: None,
                    preferred_until: None,
                    expires_at: None,
                },
            );
        });

        assert_eq!(
            routes.lookup(&ADDR_1B.into(), Instant::from_millis(0)),
            Some(ADDR_1B.into())
        );
    }

    #[cfg(all(feature = "alloc", feature = "proto-ipv4"))]
    #[test]
    fn alloc_route_storage_is_not_limited_by_iface_max_route_count() {
        let mut routes = Routes::new();
        routes.update(|storage| {
            for third_octet in 0..=crate::config::IFACE_MAX_ROUTE_COUNT {
                storage.push(Route {
                    cidr: Ipv4Cidr::new(Ipv4Address::new(198, 18, third_octet as u8, 0), 24).into(),
                    via_router: None,
                    preferred_until: None,
                    expires_at: None,
                });
            }
        });

        assert_eq!(
            routes.lookup(
                &Ipv4Address::new(198, 18, crate::config::IFACE_MAX_ROUTE_COUNT as u8, 1).into(),
                Instant::from_millis(0)
            ),
            Some(Ipv4Address::new(198, 18, crate::config::IFACE_MAX_ROUTE_COUNT as u8, 1).into())
        );
    }
}
