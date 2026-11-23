//! Router discovery transmission. RTNL serializes address transitions, sends
//! and RA application. The receive path snapshots the latest sent series.

use alloc::sync::Arc;
use smoltcp::wire::{IpAddress, Ipv6Address, NdiscRepr, RawHardwareAddress};

use crate::{
    driver::net::{
        types::{InterfaceFlags, InterfaceType},
        Iface, IfaceCommon,
    },
    net::route::is_ipv6_link_local,
    process::namespace::net_namespace::NetNamespace,
    time::Instant,
};

use super::ipv6_autoconf::{carrier_ready, changed, send_ndisc};

const INITIAL_US: i64 = 4_000_000;
const MAX_US: i64 = 3_600_000_000;
const LAST_WAIT_US: i64 = 1_000_000;
const ALL_ROUTERS: Ipv6Address = Ipv6Address::new(0xff02, 0, 0, 0, 0, 0, 0, 2);

#[derive(Clone, Copy, Debug)]
struct Series {
    deadline: i64,
    interval: i64,
    sent: u32,
}

#[derive(Debug)]
pub(super) struct State {
    pub(super) limit: i32,
    pending: bool,
    active: Option<Series>,
    generation: u64,
    sent_on_link: bool,
    heard_router: bool,
}

impl Default for State {
    fn default() -> Self {
        Self {
            limit: -1,
            pending: false,
            active: None,
            generation: 0,
            sent_on_link: false,
            heard_router: false,
        }
    }
}

impl State {
    pub(super) fn link_reset(&mut self) {
        self.pending = false;
        self.active = None;
        self.sent_on_link = false;
        self.heard_router = false;
    }
}

fn source(iface: &dyn Iface) -> Option<Ipv6Address> {
    iface
        .router_common()
        .ip_addrs
        .read()
        .iter()
        .find_map(|cidr| match cidr.address() {
            IpAddress::Ipv6(address) if is_ipv6_link_local(address.into()) => Some(address),
            _ => None,
        })
}

/// Called after publishing the first or last usable link-local address.
pub(in crate::net) fn address_changed(iface: &dyn Iface, present: bool) {
    if iface.type_() != InterfaceType::ETHER {
        return;
    }
    let common = iface.common();
    let allowed = common.accepts_router_advertisements();
    let mut device = common.ipv6_autoconf.lock();
    let state = &mut device.solicitation;
    state.pending = present && allowed && state.limit != 0;
    if !present {
        state.active = None;
    }
    drop(device);
    changed(common);
}

pub(in crate::net) fn link_up(iface: &dyn Iface) {
    if source(iface).is_some() {
        address_changed(iface, true);
    }
}

pub(in crate::net) fn sent_generation(common: &IfaceCommon) -> Option<u64> {
    let device = common.ipv6_autoconf.lock();
    let state = &device.solicitation;
    state.sent_on_link.then_some(state.generation)
}

pub(in crate::net) fn accept_advertisement(common: &IfaceCommon, generation: u64) {
    let mut device = common.ipv6_autoconf.lock();
    let state = &mut device.solicitation;
    if state.generation == generation && state.sent_on_link {
        state.heard_router = true;
        state.active = None;
    }
}

pub(super) fn poll(netns: &Arc<NetNamespace>, iface: &Arc<dyn Iface>) -> Option<i64> {
    let common = iface.common();
    if !iface.flags().contains(InterfaceFlags::UP) || !common.ipv6_enabled() {
        return None;
    }
    let now = Instant::now().total_micros();
    let allowed = common.accepts_router_advertisements();
    let ready = carrier_ready(iface.as_ref());
    let (mut next_series, wait, first) = {
        let mut device = common.ipv6_autoconf.lock();
        let state = &mut device.solicitation;
        if state.pending && ready {
            state.pending = false;
            if !allowed || state.limit == 0 {
                return None;
            }
            let interval = jitter(INITIAL_US, 1);
            (
                Series {
                    deadline: now,
                    interval,
                    sent: 1,
                },
                interval,
                true,
            )
        } else {
            let mut series = state.active?;
            if series.deadline > now {
                return Some(series.deadline);
            }
            if !allowed
                || state.heard_router
                || (state.limit >= 0 && series.sent >= state.limit as u32)
            {
                state.active = None;
                return None;
            }
            series.sent = series.sent.saturating_add(1);
            let next = jitter(series.interval, 2);
            series.interval = if next > MAX_US {
                jitter(MAX_US, 1)
            } else {
                next
            };
            let wait = if series.sent == state.limit as u32 && state.limit > 0 {
                LAST_WAIT_US
            } else {
                series.interval
            };
            (series, wait, false)
        }
    };

    let Some(src) = source(iface.as_ref()) else {
        common.ipv6_autoconf.lock().solicitation.active = None;
        return None;
    };
    if first {
        let mut device = common.ipv6_autoconf.lock();
        let state = &mut device.solicitation;
        state.generation = state.generation.wrapping_add(1);
        state.sent_on_link = false;
    }
    let _ = send_ndisc(
        netns,
        iface.as_ref(),
        src,
        ALL_ROUTERS,
        NdiscRepr::RouterSolicit {
            lladdr: Some(RawHardwareAddress::from_bytes(iface.mac().as_bytes())),
        },
        // Receive-side RA processing can observe the peer's reply before
        // this call returns. Publish the generation at TX queue admission.
        || common.ipv6_autoconf.lock().solicitation.sent_on_link = true,
    );
    next_series.deadline = Instant::now().total_micros().saturating_add(wait);
    common.ipv6_autoconf.lock().solicitation.active = Some(next_series);
    Some(next_series.deadline)
}

fn jitter(interval: i64, multiplier: i64) -> i64 {
    let mut random = [0; 4];
    crate::libs::rand::fill_random_bytes(&mut random);
    let factor = multiplier * 1000 + i64::from(u32::from_ne_bytes(random) % 201) - 100;
    interval * factor / 1000
}
