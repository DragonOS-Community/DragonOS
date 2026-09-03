use alloc::sync::Arc;

use smoltcp::wire::{IpAddress, IpEndpoint, IpListenEndpoint};
use system_error::SystemError;

use crate::{net::route::Ipv4OutputFlow, process::namespace::net_namespace::NetNamespace};

fn needs_flow(
    local: IpListenEndpoint,
    destination: IpAddress,
    is_multicast: bool,
    is_broadcast: bool,
) -> bool {
    local.addr.is_none()
        && matches!(destination, IpAddress::Ipv4(_))
        && !is_multicast
        && !is_broadcast
}

pub(super) fn resolve_wildcard_ipv4(
    netns: &Arc<NetNamespace>,
    local: IpListenEndpoint,
    destination: IpAddress,
    required_oif: Option<u32>,
    fixed_source: Option<IpAddress>,
    is_multicast: bool,
    is_broadcast: bool,
) -> Result<Option<Ipv4OutputFlow>, SystemError> {
    if !needs_flow(local, destination, is_multicast, is_broadcast) {
        return Ok(None);
    }
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
    use smoltcp::wire::{IpAddress, IpEndpoint, IpListenEndpoint};

    use super::needs_flow;

    #[test]
    fn wildcard_ipv4_is_distinct_from_fixed_and_ipv6_endpoints() {
        let wildcard = IpListenEndpoint::from(12345);
        assert!(needs_flow(
            wildcard,
            IpAddress::v4(203, 0, 113, 1),
            false,
            false
        ));
        assert!(!needs_flow(
            wildcard,
            IpAddress::v6(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1),
            false,
            false
        ));
        assert!(!needs_flow(
            IpListenEndpoint::from(IpEndpoint::new(IpAddress::v4(192, 0, 2, 1), 12345)),
            IpAddress::v4(203, 0, 113, 1),
            false,
            false
        ));
    }
}
