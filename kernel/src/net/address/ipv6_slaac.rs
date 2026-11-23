//! Lifetimes received through an autonomous IPv6 Prefix Information Option.

const MICROS_PER_SECOND: i64 = 1_000_000;
const MIN_VALID_LIFETIME_US: i64 = 7_200 * MICROS_PER_SECOND;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Lifetimes {
    valid_until_us: Option<i64>,
    preferred_until_us: Option<i64>,
    created_at_us: i64,
    updated_at_us: i64,
    deprecation_notified: bool,
}

impl Lifetimes {
    pub(super) fn from_option(received_at_us: i64, valid_secs: u32, preferred_secs: u32) -> Self {
        let deadline = |seconds| {
            (seconds != u32::MAX)
                .then(|| received_at_us.saturating_add(i64::from(seconds) * MICROS_PER_SECOND))
        };
        Self {
            valid_until_us: deadline(valid_secs),
            preferred_until_us: deadline(preferred_secs),
            created_at_us: received_at_us,
            updated_at_us: received_at_us,
            deprecation_notified: false,
        }
    }

    pub(super) fn refresh(mut self, existing: Option<Self>) -> Self {
        let floor = self.updated_at_us.saturating_add(MIN_VALID_LIFETIME_US);
        let old_deadline = existing.and_then(|lifetimes| lifetimes.valid_until_us);
        let minimum = old_deadline.map_or(floor, |deadline| deadline.min(floor));
        if let Some(valid_until_us) = self.valid_until_us.as_mut() {
            *valid_until_us = (*valid_until_us).max(minimum);
        }
        if let Some(existing) = existing {
            self.created_at_us = existing.created_at_us;
        }
        self
    }

    pub(crate) fn preferred_until_us(self) -> Option<i64> {
        self.preferred_until_us
    }

    pub(super) fn expired(self, now_us: i64) -> bool {
        self.valid_until_us
            .is_some_and(|deadline| deadline <= now_us)
    }

    pub(crate) fn deprecated(self, now_us: i64) -> bool {
        self.preferred_until_us
            .is_some_and(|deadline| deadline <= now_us)
    }

    pub(super) fn next_deadline(self) -> Option<i64> {
        self.valid_until_us
            .into_iter()
            .chain(
                self.preferred_until_us
                    .filter(|_| !self.deprecation_notified),
            )
            .min()
    }

    pub(super) fn mark_deprecation_reported(&mut self) {
        self.deprecation_notified = true;
    }

    pub(crate) fn cache_info(self, now_us: i64) -> [u32; 4] {
        let remaining = |deadline: Option<i64>| {
            deadline.map_or(u32::MAX, |deadline| {
                let micros = deadline.saturating_sub(now_us).max(0);
                (micros.saturating_add(MICROS_PER_SECOND - 1) / MICROS_PER_SECOND) as u32
            })
        };
        [
            remaining(self.preferred_until_us),
            remaining(self.valid_until_us),
            (self.created_at_us / 10_000) as u32,
            (self.updated_at_us / 10_000) as u32,
        ]
    }
}

use alloc::{sync::Arc, vec::Vec};
use smoltcp::wire::{IpCidr, Ipv6Cidr, NdiscPrefixInformation};
use system_error::SystemError;

use crate::{
    driver::net::Iface,
    net::{rtnl::RtnlGuard, socket::netlink::notify_iface_protocol_change},
    process::namespace::net_namespace::NetNamespace,
    time::Instant,
};

use super::{ipv6_autoconf, AddressMutation};

pub(crate) fn apply(
    rtnl: &RtnlGuard,
    netns: &Arc<NetNamespace>,
    iface: &Arc<dyn Iface>,
    prefix: &NdiscPrefixInformation,
    received_at_us: i64,
) -> Result<(), SystemError> {
    let now_us = Instant::now().total_micros();
    let Some(target) = ipv6_autoconf::address_from_mac(iface.as_ref(), prefix.prefix) else {
        return Ok(());
    };
    let offered = Lifetimes::from_option(
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
            remove(rtnl, netns, iface, cidr)?;
        } else {
            let mut refreshed = offered.refresh(lifetimes);
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
            return Ok(());
        }
    }
    if !offered.expired(now_us) {
        let address = Ipv6Cidr::new(target, 64);
        ipv6_autoconf::start_address_dad(netns, iface, address, Some(offered), now_us)?;
        ipv6_autoconf::changed(iface.common());
    }
    Ok(())
}

pub(super) fn remove(
    rtnl: &RtnlGuard,
    netns: &Arc<NetNamespace>,
    iface: &Arc<dyn Iface>,
    cidr: IpCidr,
) -> Result<(), SystemError> {
    let removed = super::address_info(iface, cidr).ok();
    let change = super::mutate_address(rtnl, iface, AddressMutation::Delete(cidr))?;
    notify_iface_protocol_change(
        netns,
        iface,
        removed,
        [],
        Some(change.route_changes),
        Vec::new(),
    );
    Ok(())
}

pub(super) fn poll(
    rtnl: &RtnlGuard,
    netns: &Arc<NetNamespace>,
    iface: &Arc<dyn Iface>,
    now_us: i64,
) -> Option<i64> {
    loop {
        let mut metadata = iface.common().address_metadata().lock();
        let due = metadata
            .iter_mut()
            .filter_map(|entry| {
                let deadline = entry.lifetimes?.next_deadline()?;
                Some((entry, deadline))
            })
            .min_by_key(|&(_, deadline)| deadline);
        let (entry, deadline) = due?;
        if deadline > now_us {
            return Some(deadline);
        }
        let cidr = entry.cidr;
        let usable = entry.state.is_usable();
        let lifetimes = entry.lifetimes.as_mut().unwrap();
        let expired = lifetimes.expired(now_us);
        if !expired {
            lifetimes.mark_deprecation_reported();
        }
        drop(metadata);

        if expired {
            if remove(rtnl, netns, iface, cidr).is_err() {
                return Some(now_us.saturating_add(ipv6_autoconf::RETRY_US));
            }
        } else if usable {
            notify_iface_protocol_change(netns, iface, Vec::new(), [cidr], None, Vec::new());
        }
    }
}
