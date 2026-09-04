use alloc::sync::Arc;

use smoltcp::wire::{IpAddress, IpEndpoint, IpListenEndpoint};
use system_error::SystemError;

use crate::{net::route::Ipv4OutputFlow, process::namespace::net_namespace::NetNamespace};

use super::{EphemeralBindTarget, UdpSocket};

/// Returns the Linux IPv4 output constraints supplied by socket options.
/// The SocketSet owner is deliberately absent: it is not an output-device
/// constraint and must not override the namespace FIB.
pub(super) fn socket_constraints(
    socket: &UdpSocket,
    destination: IpAddress,
) -> Result<(Option<u32>, Option<IpAddress>), SystemError> {
    let device_oif = socket
        .device_binding
        .resolve_iface(&socket.netns)?
        .map(|iface| iface.nic_id() as u32);
    if !matches!(destination, IpAddress::Ipv4(address) if address.is_multicast()) {
        return Ok((device_oif, None));
    }

    let multicast_oif = socket
        .ip_multicast_ifindex
        .load(core::sync::atomic::Ordering::Relaxed);
    let multicast_addr = socket
        .ip_multicast_addr
        .load(core::sync::atomic::Ordering::Relaxed);
    let required_oif = device_oif.or_else(|| {
        u32::try_from(multicast_oif)
            .ok()
            .filter(|ifindex| *ifindex != 0)
    });
    let fixed_source = (multicast_addr != 0).then(|| {
        let octets = multicast_addr.to_ne_bytes();
        IpAddress::v4(octets[0], octets[1], octets[2], octets[3])
    });
    Ok((required_oif, fixed_source))
}

pub(super) fn ephemeral_target(
    socket: &UdpSocket,
    flow: Ipv4OutputFlow,
) -> Result<EphemeralBindTarget, SystemError> {
    let iface = socket
        .netns
        .device_list()
        .get(&(flow.oif as usize))
        .cloned()
        .ok_or(SystemError::ENETUNREACH)?;
    crate::net::socket::inet::common::ephemeral_target_for_source(
        &socket.netns,
        iface,
        flow.source,
        SystemError::ENETUNREACH,
    )
}

fn needs_flow(destination: IpAddress) -> bool {
    matches!(destination, IpAddress::Ipv4(_))
}

pub(super) fn resolve_ipv4_send_flow(
    netns: &Arc<NetNamespace>,
    local: IpListenEndpoint,
    destination: IpAddress,
    required_oif: Option<u32>,
    fixed_source: Option<IpAddress>,
) -> Result<Option<Ipv4OutputFlow>, SystemError> {
    if !needs_flow(destination) {
        return Ok(None);
    }
    let fixed_source = local
        .addr
        .filter(|address| !address.is_unspecified())
        .or(fixed_source);
    crate::net::route::resolve_ipv4_output_flow(netns, destination, required_oif, fixed_source)
        .map(Some)
}

pub(super) fn local_source_endpoint(
    netns: &Arc<NetNamespace>,
    local: IpListenEndpoint,
    destination: IpAddress,
    local_delivery_ifindex: Option<i32>,
    output_flow: Option<Ipv4OutputFlow>,
    unspecified: IpAddress,
) -> IpEndpoint {
    let address = local
        .addr
        .filter(|address| !address.is_unspecified())
        .or_else(|| output_flow.map(|flow| flow.source))
        .or_else(|| {
            let ifindex = usize::try_from(local_delivery_ifindex?).ok()?;
            let iface = netns.device_list().get(&ifindex).cloned()?;
            crate::net::socket::inet::common::pick_configured_source_addr(&iface, &destination)
        })
        .unwrap_or(unspecified);
    IpEndpoint::new(address, local.port)
}

#[cfg(test)]
mod tests {
    use smoltcp::wire::IpAddress;

    use super::needs_flow;

    #[test]
    fn every_ipv4_send_requires_a_current_flow() {
        assert!(needs_flow(IpAddress::v4(203, 0, 113, 1)));
        assert!(needs_flow(IpAddress::v4(239, 1, 2, 3)));
        assert!(needs_flow(IpAddress::v4(255, 255, 255, 255)));
        assert!(!needs_flow(IpAddress::v6(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1)));
    }
}
