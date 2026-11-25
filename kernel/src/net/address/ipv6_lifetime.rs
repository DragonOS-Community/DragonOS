//! Monotonic IPv6 address deadlines, shared by RTNL configuration and SLAAC.

use alloc::{sync::Arc, vec::Vec};
use smoltcp::wire::IpCidr;
use system_error::SystemError;

use crate::{
    driver::net::{AddressState, Iface},
    net::{rtnl::RtnlGuard, socket::netlink::notify_iface_protocol_change},
    process::namespace::net_namespace::NetNamespace,
};

use super::{ipv6_autoconf, AddressMutation};

const MICROS_PER_SECOND: i64 = 1_000_000;
const MIN_VALID_LIFETIME_US: i64 = 7_200 * MICROS_PER_SECOND;

pub(in crate::net) struct ConfiguredLifetimes {
    pub preferred_secs: u32,
    pub valid_secs: u32,
}

impl ConfiguredLifetimes {
    pub(super) fn into_deadlines(self) -> Result<Lifetimes, SystemError> {
        if self.valid_secs == 0 || self.preferred_secs > self.valid_secs {
            return Err(SystemError::EINVAL);
        }
        let mut lifetimes = Lifetimes::from_seconds(
            crate::time::Instant::now().total_micros(),
            self.valid_secs,
            self.preferred_secs,
        );
        if self.preferred_secs == 0 {
            lifetimes.mark_deprecation_reported();
        }
        Ok(lifetimes)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Lifetimes {
    valid_until_us: Option<i64>,
    preferred_until_us: Option<i64>,
    created_at_us: i64,
    updated_at_us: i64,
    deprecation_notified: bool,
}

impl Lifetimes {
    pub(super) fn from_seconds(received_at_us: i64, valid_secs: u32, preferred_secs: u32) -> Self {
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

    pub(super) fn refresh_from_ra(mut self, existing: Option<Self>) -> Self {
        let floor = self.updated_at_us.saturating_add(MIN_VALID_LIFETIME_US);
        let old_deadline = existing.and_then(|lifetimes| lifetimes.valid_until_us);
        let minimum = old_deadline.map_or(floor, |deadline| deadline.min(floor));
        if let Some(valid_until_us) = self.valid_until_us.as_mut() {
            *valid_until_us = (*valid_until_us).max(minimum);
        }
        self.replacing(existing)
    }

    pub(super) fn replacing(mut self, existing: Option<Self>) -> Self {
        if let Some(existing) = existing {
            self.created_at_us = existing.created_at_us;
        }
        self
    }

    pub(super) fn valid_indefinitely(self) -> bool {
        self.valid_until_us.is_none()
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

pub(super) fn remove(
    rtnl: &RtnlGuard,
    netns: &Arc<NetNamespace>,
    iface: &Arc<dyn Iface>,
    cidr: IpCidr,
    removal_state: Option<AddressState>,
) -> Result<(), SystemError> {
    let removed = super::address_info(iface, cidr).ok().map(|mut info| {
        if let Some(state) = removal_state {
            info.state = state;
        }
        info
    });
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
        let notify_deprecation = entry.state.is_usable()
            || matches!(
                entry.state,
                AddressState::ManualDad(_) | AddressState::Retained(_)
            );
        let lifetimes = entry.lifetimes.as_mut().unwrap();
        let expired = lifetimes.expired(now_us);
        if !expired {
            lifetimes.mark_deprecation_reported();
        }
        drop(metadata);

        if expired {
            if remove(rtnl, netns, iface, cidr, None).is_err() {
                return Some(now_us.saturating_add(ipv6_autoconf::RETRY_US));
            }
        } else if notify_deprecation {
            notify_iface_protocol_change(netns, iface, Vec::new(), [cidr], None, Vec::new());
        }
    }
}
