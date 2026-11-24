//! Lifetimes received through an autonomous IPv6 Prefix Information Option.

use alloc::{sync::Arc, vec::Vec};
use smoltcp::wire::{Ipv6Cidr, NdiscPrefixInformation};
use system_error::SystemError;

use crate::{
    driver::net::Iface,
    net::{rtnl::RtnlGuard, socket::netlink::notify_iface_protocol_change},
    process::namespace::net_namespace::NetNamespace,
    time::Instant,
};

use super::{ipv6_autoconf, ipv6_lifetime};

/// Returns false when the device cannot generate an interface identifier.
pub(crate) fn apply(
    rtnl: &RtnlGuard,
    netns: &Arc<NetNamespace>,
    iface: &Arc<dyn Iface>,
    prefix: &NdiscPrefixInformation,
    received_at_us: i64,
) -> Result<bool, SystemError> {
    let now_us = Instant::now().total_micros();
    let Some(target) = ipv6_autoconf::address_from_mac(iface.as_ref(), prefix.prefix) else {
        return Ok(false);
    };
    let offered = ipv6_lifetime::Lifetimes::from_seconds(
        received_at_us,
        prefix.valid_lifetime.secs() as u32,
        prefix.preferred_lifetime.secs() as u32,
    );
    let existing = iface
        .common()
        .address_metadata()
        .lock()
        .iter()
        .find(|entry| entry.cidr.address() == target.into())
        .map(|entry| (entry.cidr, entry.state, entry.lifetimes));
    if let Some((cidr, state, lifetimes)) = existing {
        if lifetimes.is_some_and(|current| current.expired(now_us)) {
            ipv6_lifetime::remove(rtnl, netns, iface, cidr)?;
        } else {
            let mut refreshed = offered.refresh_from_ra(lifetimes);
            if refreshed.deprecated(now_us) {
                refreshed.mark_deprecation_reported();
            }
            {
                // NAT captures the preferences with its FIB read view.
                let router = netns.router();
                let _fib = router.fib_write();
                let mut smol_iface = iface.smol_iface().lock();
                let mut metadata = iface.common().address_metadata().lock();
                let entry = metadata
                    .iter_mut()
                    .find(|entry| entry.cidr == cidr)
                    .ok_or(SystemError::EADDRNOTAVAIL)?;
                entry.lifetimes = Some(refreshed);
                super::publish_ipv6_preference(&mut smol_iface, &metadata);
            }
            if state.is_usable() {
                notify_iface_protocol_change(netns, iface, Vec::new(), [cidr], None, Vec::new());
            }
            ipv6_autoconf::changed(iface.common());
            return Ok(true);
        }
    }
    if !offered.expired(now_us) {
        let address = Ipv6Cidr::new(target, 64);
        ipv6_autoconf::start_address_dad(netns, iface, address, Some(offered), now_us)?;
        ipv6_autoconf::changed(iface.common());
    }
    Ok(true)
}
