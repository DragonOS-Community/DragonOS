use alloc::sync::Arc;

use smoltcp::wire::{IpAddress, IpCidr};
use system_error::SystemError;

use crate::{
    driver::net::{types::InterfaceFlags, Iface},
    net::address::{iface_has_address, netns_has_address, source_address_usable_on_iface},
    process::namespace::net_namespace::NetNamespace,
};

use super::{
    is_ipv4, is_ipv6_link_local, same_family_option, RouteEntry, RTN_UNICAST, RT_SCOPE_LINK,
};

pub(super) fn validate_entry(
    netns: &Arc<NetNamespace>,
    route: RouteEntry,
) -> Result<(), SystemError> {
    let iface = netns
        .device_list()
        .get(&(route.oif as usize))
        .cloned()
        .ok_or(SystemError::ENODEV)?;
    validate_entry_on_iface(netns, &iface, route)
}

pub(super) fn validate_entry_on_iface(
    netns: &Arc<NetNamespace>,
    iface: &Arc<dyn Iface>,
    route: RouteEntry,
) -> Result<(), SystemError> {
    if route.source.is_some() {
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }
    if route.kind != RTN_UNICAST {
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }
    if !same_family_option(route.destination.address(), route.gateway)
        || !same_family_option(route.destination.address(), route.preferred_source)
    {
        return Err(SystemError::EINVAL);
    }
    if !is_ipv4(route.destination.address()) && route.tos != 0 {
        return Err(SystemError::EINVAL);
    }
    let expected_oif = u32::try_from(iface.nic_id()).map_err(|_| SystemError::EOVERFLOW)?;
    if route.oif != expected_oif {
        return Err(SystemError::ENODEV);
    }
    if !iface.flags().contains(InterfaceFlags::UP) {
        return Err(SystemError::ENETDOWN);
    }
    if let Some(source) = route.preferred_source {
        if !source_address_usable_on_iface(netns, iface, source) {
            return Err(SystemError::EINVAL);
        }
    }
    if route.nexthop_flags != 0 && route.scope >= RT_SCOPE_LINK {
        return Err(SystemError::EINVAL);
    }
    if route.gateway.is_some_and(|gateway| !gateway.is_unicast()) {
        return Err(SystemError::EINVAL);
    }
    Ok(())
}

pub(super) fn validate_gateway_iface(
    netns: &Arc<NetNamespace>,
    gateway: IpAddress,
    oif: u32,
    onlink: bool,
) -> Result<u32, SystemError> {
    let iface = netns
        .device_list()
        .get(&(oif as usize))
        .cloned()
        .ok_or(SystemError::ENODEV)?;
    if !is_ipv4(gateway) {
        let gateway_is_local = if is_ipv6_link_local(gateway) {
            iface_has_address(&iface, gateway)
        } else {
            netns_has_address(netns, gateway)
        };
        if iface.flags().contains(InterfaceFlags::LOOPBACK) || gateway_is_local {
            return Err(SystemError::EINVAL);
        }
    }
    if let IpAddress::Ipv4(gateway) = gateway {
        if onlink && iface_has_address(&iface, IpAddress::Ipv4(gateway)) {
            return Err(SystemError::EINVAL);
        }
        let invalid_endpoint = iface.common().ip_addrs().iter().any(|cidr| {
            let IpCidr::Ipv4(cidr) = cidr else {
                return false;
            };
            cidr.prefix_len() < 31
                && cidr.contains_addr(&gateway)
                && cidr
                    .broadcast()
                    .is_some_and(|broadcast| gateway.octets() == broadcast.octets())
        });
        if invalid_endpoint {
            return Err(SystemError::EINVAL);
        }
    }
    Ok(oif)
}
