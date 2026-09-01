use alloc::vec::Vec;

use smoltcp::wire::{IpAddress, IpCidr, Ipv6AddressExt, Ipv6Cidr};

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
pub(crate) const RTN_MULTICAST: u8 = 5;

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

#[derive(Debug, Clone, Copy)]
pub(crate) struct RouteLookupResult {
    pub oif: u32,
    pub next_hop: IpAddress,
    pub source: RouteSourcePolicy,
    pub table: u32,
    pub matched: RouteEntry,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum RouteSourcePolicy {
    SelectConfigured,
    Preferred(IpAddress),
    AllowUnspecified,
}

impl RouteSourcePolicy {
    pub(crate) fn preferred(self) -> Option<IpAddress> {
        match self {
            Self::Preferred(source) => Some(source),
            Self::SelectConfigured | Self::AllowUnspecified => None,
        }
    }
}

impl RouteLookupResult {
    /// Constructs Linux's ephemeral IPv4 output decision for a caller that
    /// supplied an OIF but whose normal FIB lookup found no route. This value
    /// is never inserted into the authoritative FIB.
    pub(super) fn direct_ipv4_output(destination: smoltcp::wire::Ipv4Address, oif: u32) -> Self {
        Self::transient_ipv4(
            destination,
            oif,
            RT_TABLE_MAIN,
            RT_SCOPE_UNIVERSE,
            RTN_UNICAST,
            RouteSourcePolicy::AllowUnspecified,
        )
    }

    /// Constructs the non-persistent decision used for IPv4 limited
    /// broadcast ingress/output. Linux exposes this only as a lookup result,
    /// never as a local-table FIB object.
    pub(super) fn limited_broadcast(oif: u32) -> Self {
        Self::transient_ipv4(
            smoltcp::wire::Ipv4Address::new(255, 255, 255, 255),
            oif,
            RT_TABLE_MAIN,
            RT_SCOPE_LINK,
            RTN_BROADCAST,
            RouteSourcePolicy::SelectConfigured,
        )
    }

    pub(super) fn into_limited_broadcast(mut self) -> Self {
        self.reclassify_ipv4(
            smoltcp::wire::Ipv4Address::new(255, 255, 255, 255),
            RTN_BROADCAST,
            None,
        );
        self
    }

    pub(super) fn into_multicast(mut self, destination: smoltcp::wire::Ipv4Address) -> Self {
        // Linux discards a default/less-specific-than-224/4 gateway when it
        // synthesizes the multicast result, but keeps a gateway selected by
        // a route that specifically covers multicast space.
        let gateway = (self.matched.destination.prefix_len() >= 4)
            .then_some(self.matched.gateway)
            .flatten();
        self.reclassify_ipv4(destination, RTN_MULTICAST, gateway);
        self
    }

    fn transient_ipv4(
        destination: smoltcp::wire::Ipv4Address,
        oif: u32,
        table: u32,
        scope: u8,
        kind: u8,
        source: RouteSourcePolicy,
    ) -> Self {
        let destination = IpCidr::Ipv4(smoltcp::wire::Ipv4Cidr::new(destination, 32));
        let matched = RouteEntry {
            destination,
            source: None,
            preferred_source: None,
            table,
            priority: 0,
            tos: 0,
            protocol: 0,
            scope,
            kind,
            oif,
            gateway: None,
            nexthop_flags: 0,
        };
        Self {
            oif,
            next_hop: destination.address(),
            source,
            table,
            matched,
        }
    }

    /// Reclassifies an authoritative FIB winner into Linux's transient IPv4
    /// broadcast/multicast output result without changing its table, OIF or
    /// source-selection policy.
    fn reclassify_ipv4(
        &mut self,
        destination: smoltcp::wire::Ipv4Address,
        kind: u8,
        gateway: Option<IpAddress>,
    ) {
        let destination = IpCidr::Ipv4(smoltcp::wire::Ipv4Cidr::new(destination, 32));
        self.next_hop = gateway.unwrap_or_else(|| destination.address());
        self.matched.destination = destination;
        self.matched.kind = kind;
        self.matched.scope = RT_SCOPE_LINK;
        self.matched.gateway = gateway;
    }
}

pub(super) fn is_limited_broadcast(address: IpAddress) -> bool {
    matches!(address, IpAddress::Ipv4(address) if address.octets() == [255, 255, 255, 255])
}

pub(super) fn ipv4_multicast(address: IpAddress) -> Option<smoltcp::wire::Ipv4Address> {
    match address {
        IpAddress::Ipv4(address) if address.is_multicast() => Some(address),
        _ => None,
    }
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

/// Route notifications required by the Linux-facing control plane.
///
/// This is deliberately not a raw FIB diff: Linux silently purges ordinary
/// IPv4 routes on NETDEV_DOWN and silently clears surviving IPv6 preferred
/// sources after address deletion.
#[derive(Debug, Default)]
pub(crate) struct RouteNotifications {
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

pub(crate) fn canonical_cidr(cidr: IpCidr) -> IpCidr {
    match cidr {
        IpCidr::Ipv4(cidr) => IpCidr::Ipv4(cidr.network()),
        IpCidr::Ipv6(cidr) => IpCidr::Ipv6(Ipv6Cidr::new(
            cidr.address().mask(cidr.prefix_len()).into(),
            cidr.prefix_len(),
        )),
    }
}

pub(super) fn same_family(left: IpAddress, right: IpAddress) -> bool {
    matches!(
        (left, right),
        (IpAddress::Ipv4(_), IpAddress::Ipv4(_)) | (IpAddress::Ipv6(_), IpAddress::Ipv6(_))
    )
}

pub(super) fn same_family_option(reference: IpAddress, candidate: Option<IpAddress>) -> bool {
    candidate.is_none_or(|candidate| same_family(reference, candidate))
}

pub(super) fn is_ipv4(address: IpAddress) -> bool {
    matches!(address, IpAddress::Ipv4(_))
}

pub(super) fn is_ipv6_link_local(address: IpAddress) -> bool {
    let IpAddress::Ipv6(address) = address else {
        return false;
    };
    let octets = address.octets();
    octets[0] == 0xfe && octets[1] & 0xc0 == 0x80
}
