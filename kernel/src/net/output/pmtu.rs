//! Socket PMTU policy and typed output results over the common admission path.
use super::*;

#[derive(Debug)]
pub(crate) struct PmtuOutputError {
    pub(crate) error: SystemError,
    pub(crate) mtu: Option<usize>,
}

impl From<SystemError> for PmtuOutputError {
    fn from(error: SystemError) -> Self {
        Self { error, mtu: None }
    }
}

/// Actual post-NAT addresses, not a retained FIB/device snapshot. A socket
/// revalidates its connection and current route before using this hint.
#[derive(Clone, Copy, Debug)]
pub(crate) struct SocketRouteHint {
    pub(crate) source: IpAddress,
    pub(crate) destination: IpAddress,
    pub(crate) rules_generation: u32,
}

pub(super) fn save_route_hint(
    bytes: &[u8],
    version: IpVersion,
    generation: u32,
    hint: Option<&mut Option<SocketRouteHint>>,
) -> Result<(), SystemError> {
    let Some(hint) = hint else {
        return Ok(());
    };
    let source = match version {
        IpVersion::Ipv4 => IpAddress::Ipv4(
            Ipv4Packet::new_checked(bytes)
                .map_err(|_| SystemError::EINVAL)?
                .src_addr(),
        ),
        IpVersion::Ipv6 => IpAddress::Ipv6(
            smoltcp::wire::Ipv6Packet::new_checked(bytes)
                .map_err(|_| SystemError::EINVAL)?
                .src_addr(),
        ),
    };
    *hint = Some(SocketRouteHint {
        source,
        destination: destination(bytes, version)?,
        rules_generation: generation,
    });
    Ok(())
}

#[derive(Clone, Copy)]
pub(super) struct SocketMtuPolicy {
    pub(super) discovery: crate::net::socket::inet::common::pmtu::PmtuPolicy,
    pub(super) ceiling: Option<usize>,
}

/// Socket output controls stay together rather than adding positional flags
/// to the common OUTPUT/POST_ROUTING admission contract.
pub(super) struct SocketOutput<'a> {
    pub(super) policy: Option<SocketMtuPolicy>,
    pub(super) multicast_loop: bool,
    pub(super) may_fragment: bool,
    pub(super) hint: Option<&'a mut Option<SocketRouteHint>>,
}

pub(crate) struct RawOutputPolicy {
    pub(crate) discovery: crate::net::socket::inet::common::pmtu::PmtuPolicy,
    pub(crate) ceiling: Option<usize>,
    pub(crate) multicast_loop: bool,
    pub(crate) hdrincl: bool,
}

pub(super) fn apply_socket_mtu(
    netns: &Arc<NetNamespace>,
    reservation: &mut PreparedIpOutputReservation<'_>,
    mut route: OutputRouteDecision,
    policy: SocketMtuPolicy,
    version: IpVersion,
) -> Result<OutputRouteDecision, PmtuOutputError> {
    let (source, destination) = match version {
        IpVersion::Ipv4 => {
            let packet =
                Ipv4Packet::new_checked(reservation.bytes()).map_err(|_| SystemError::EINVAL)?;
            (
                IpAddress::Ipv4(packet.src_addr()),
                IpAddress::Ipv4(packet.dst_addr()),
            )
        }
        IpVersion::Ipv6 => {
            let packet = smoltcp::wire::Ipv6Packet::new_checked(reservation.bytes())
                .map_err(|_| SystemError::EINVAL)?;
            (
                IpAddress::Ipv6(packet.src_addr()),
                IpAddress::Ipv6(packet.dst_addr()),
            )
        }
    };
    let learned = crate::net::route::pmtu::path_mtu(netns, route, source, destination);
    route.ip_mtu = policy
        .discovery
        .effective_mtu(learned.interface, learned.path);
    if let Some(ceiling) = policy.ceiling {
        route.ip_mtu = route.ip_mtu.min(ceiling);
    }
    if reservation.bytes().len() > route.ip_mtu && !policy.discovery.allows_fragmentation() {
        return Err(PmtuOutputError {
            error: SystemError::EMSGSIZE,
            mtu: Some(route.ip_mtu),
        });
    }
    if version == IpVersion::Ipv4 {
        let df = policy
            .discovery
            .ipv4_df(reservation.bytes().len(), route.ip_mtu, learned.locked);
        let mut packet = Ipv4Packet::new_unchecked(reservation.bytes_mut());
        packet.set_dont_frag(df);
        if !df && packet.ident() == 0 {
            packet.set_ident(netns.next_ipv4_identification());
        }
        packet.fill_checksum();
    }
    Ok(route)
}

pub(crate) fn submit_prepared_ipv6_with_pmtu(
    netns: &Arc<NetNamespace>,
    reservation: PreparedIpOutputReservation<'_>,
    route: OutputRouteDecision,
    discovery: crate::net::socket::inet::common::pmtu::PmtuPolicy,
    ceiling: Option<usize>,
    multicast_loop: bool,
) -> Result<(), PmtuOutputError> {
    submit_ipv6(
        netns,
        reservation,
        route,
        None,
        SocketOutput {
            policy: Some(SocketMtuPolicy { discovery, ceiling }),
            multicast_loop,
            may_fragment: true,
            hint: None,
        },
    )
}

pub(crate) fn submit_prepared_ipv4_with_pmtu(
    netns: &Arc<NetNamespace>,
    reservation: PreparedIpOutputReservation<'_>,
    route: OutputRouteDecision,
    multicast_loop: bool,
    may_fragment: bool,
    discovery: crate::net::socket::inet::common::pmtu::PmtuPolicy,
) -> Result<Option<Arc<dyn Iface>>, PmtuOutputError> {
    submit_ipv4(
        netns,
        reservation,
        route,
        None,
        SocketOutput {
            multicast_loop,
            may_fragment,
            policy: Some(SocketMtuPolicy {
                discovery,
                ceiling: None,
            }),
            hint: None,
        },
    )
}

pub(crate) fn submit_prepared_ipv4_raw(
    netns: &Arc<NetNamespace>,
    reservation: PreparedIpOutputReservation<'_>,
    route: OutputRouteDecision,
    options: RawOutputPolicy,
    hint: &mut Option<SocketRouteHint>,
) -> Result<Option<Arc<dyn Iface>>, PmtuOutputError> {
    submit_ipv4(
        netns,
        reservation,
        route,
        None,
        SocketOutput {
            multicast_loop: options.multicast_loop,
            may_fragment: !options.hdrincl,
            policy: (!options.hdrincl).then_some(SocketMtuPolicy {
                discovery: options.discovery,
                ceiling: None,
            }),
            hint: Some(hint),
        },
    )
}

pub(crate) fn submit_prepared_ipv6_raw(
    netns: &Arc<NetNamespace>,
    reservation: PreparedIpOutputReservation<'_>,
    route: OutputRouteDecision,
    options: RawOutputPolicy,
    hint: &mut Option<SocketRouteHint>,
) -> Result<(), PmtuOutputError> {
    submit_ipv6(
        netns,
        reservation,
        route,
        None,
        SocketOutput {
            policy: (!options.hdrincl).then_some(SocketMtuPolicy {
                discovery: options.discovery,
                ceiling: options.ceiling,
            }),
            multicast_loop: options.multicast_loop,
            may_fragment: !options.hdrincl,
            hint: Some(hint),
        },
    )
}
