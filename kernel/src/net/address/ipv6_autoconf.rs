//! Namespace-poller IPv6 address configuration. RTNL serializes address
//! changes. The receive path only latches a conflict against a live probe.

use alloc::{sync::Arc, vec::Vec};
use smoltcp::phy::ChecksumCapabilities;
use smoltcp::wire::{
    Icmpv6Packet, Icmpv6Repr, IpCidr, IpProtocol, IpVersion, Ipv6Address, Ipv6AddressExt, Ipv6Cidr,
    Ipv6Packet, Ipv6Repr, NdiscNeighborFlags, NdiscRepr,
};
use system_error::SystemError;

use crate::driver::net::{
    local_output::reserve_prepared_ip_output,
    types::{InterfaceFlags, InterfaceType},
    AddressState, DadState, Iface, IfaceCommon, Operstate,
};
use crate::net::{
    ipv6_defrag::{fragment_offset, DefragDomain},
    output::submit_prepared_ipv6_before_enqueue,
    route::{OutputRouteDecision, RTN_MULTICAST, RT_TABLE_LOCAL},
    rtnl,
};
use crate::process::namespace::net_namespace::NetNamespace;
use crate::time::Instant;

use super::{ipv6_lifetime, ipv6_router_solicitation as solicitation};

const RETRANS_US: i64 = 1_000_000;
pub(super) const RETRY_US: i64 = 100_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Probe {
    address: Ipv6Cidr,
    manual: bool,
    nonce: [u8; 6],
    due: i64,
    remaining: u32,
    conflict: bool,
    failed: bool,
}

impl Probe {
    fn target(&self) -> Ipv6Address {
        self.address.address()
    }

    fn can_progress(&self, carrier: bool, dad_ready: bool, skip: bool) -> bool {
        let link_ready = if self.manual {
            dad_ready
        } else {
            carrier || self.target().is_link_local()
        };
        !self.failed && (link_ready || self.conflict || skip)
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum Setting {
    AddrGenMode,
    AcceptDad,
    DadTransmits,
    RouterSolicitations,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum PrefixSetting {
    AcceptRaPinfo,
    Autoconf,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct PrefixSettings {
    accept_ra_pinfo: i32,
    autoconf: i32,
}

impl Default for PrefixSettings {
    fn default() -> Self {
        Self {
            accept_ra_pinfo: 1,
            autoconf: 1,
        }
    }
}

impl PrefixSettings {
    pub(crate) fn get(self, setting: PrefixSetting) -> i32 {
        match setting {
            PrefixSetting::AcceptRaPinfo => self.accept_ra_pinfo,
            PrefixSetting::Autoconf => self.autoconf,
        }
    }

    pub(crate) fn set(&mut self, setting: PrefixSetting, value: i32) {
        match setting {
            PrefixSetting::AcceptRaPinfo => self.accept_ra_pinfo = value,
            PrefixSetting::Autoconf => self.autoconf = value,
        }
    }
}

#[derive(Debug)]
pub(crate) struct DeviceState {
    pub(crate) prefixes: PrefixSettings,
    pub(super) solicitation: solicitation::State,
    mode: i32,
    accept_dad: i32,
    dad_transmits: i32,
    ready_seen: bool,
    dad_ready: bool,
    retry: Option<i64>,
    probes: Vec<Probe>,
}

impl Default for DeviceState {
    fn default() -> Self {
        Self {
            prefixes: PrefixSettings::default(),
            solicitation: solicitation::State::default(),
            mode: 0,
            accept_dad: 1,
            dad_transmits: 1,
            ready_seen: false,
            dad_ready: false,
            retry: None,
            probes: Vec::new(),
        }
    }
}

impl Setting {
    pub(crate) fn read(self, common: &IfaceCommon) -> i32 {
        let state = common.ipv6_autoconf.lock();
        match self {
            Self::AddrGenMode => state.mode,
            Self::AcceptDad => state.accept_dad,
            Self::DadTransmits => state.dad_transmits,
            Self::RouterSolicitations => state.solicitation.limit,
        }
    }

    pub(crate) fn write(self, common: &IfaceCommon, value: i32) -> Result<(), SystemError> {
        let mut state = common.ipv6_autoconf.lock();
        match self {
            Self::AddrGenMode if matches!(value, 0 | 1) => {
                if state.mode != value {
                    state.mode = value;
                    state.ready_seen = false;
                    state.retry = None;
                }
            }
            Self::AddrGenMode if matches!(value, 2 | 3) => {
                return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP)
            }
            Self::AcceptDad if (-1..=2).contains(&value) => state.accept_dad = value,
            Self::DadTransmits if value >= 0 => state.dad_transmits = value,
            Self::RouterSolicitations if value >= -1 => state.solicitation.limit = value,
            _ => return Err(SystemError::EINVAL),
        }
        drop(state);
        changed(common);
        Ok(())
    }
}

pub(crate) fn changed(common: &IfaceCommon) {
    if let Some(netns) = common.net_namespace() {
        netns.notify_ipv6_autoconf();
    }
}

/// Publish IPv6 readiness on link events so a brief carrier-up interval
/// counts even if the DAD worker has not yet polled the device.
pub(crate) fn link_changed(common: &IfaceCommon, operstate: Operstate) {
    if matches!(
        operstate,
        Operstate::IF_OPER_UP | Operstate::IF_OPER_UNKNOWN
    ) {
        let mut state = common.ipv6_autoconf.lock();
        if common.flags().contains(InterfaceFlags::UP)
            && common.ipv6_enabled()
            && common.mtu() >= super::IPV6_MIN_MTU
        {
            state.dad_ready = true;
        }
    }
    changed(common);
}

pub(crate) fn cancel(common: &IfaceCommon) {
    let probes = {
        let mut state = common.ipv6_autoconf.lock();
        state.retry = None;
        state.ready_seen = false;
        state.dad_ready = false;
        state.solicitation.link_reset();
        core::mem::take(&mut state.probes)
    };
    if let Some(netns) = common.net_namespace() {
        for target in probes.into_iter().map(|probe| probe.target()) {
            let _ = common.multicast_leave_ref(&netns, target.solicited_node().into());
        }
        netns.notify_ipv6_autoconf();
    }
}

pub(super) fn cancel_target(common: &IfaceCommon, target: Ipv6Address) {
    let joined = {
        let mut state = common.ipv6_autoconf.lock();
        if let Some(index) = state
            .probes
            .iter()
            .position(|probe| probe.target() == target)
        {
            state.probes.remove(index);
            true
        } else {
            false
        }
    };
    if joined {
        if let Some(netns) = common.net_namespace() {
            let _ = common.multicast_leave_ref(&netns, target.solicited_node().into());
            netns.notify_ipv6_autoconf();
        }
    }
}

/// Moving a device discards its IPv6 configuration in the source namespace.
/// Release the pending solicited-node membership before changing ownership.
pub(crate) fn reset_for_netns_move(common: &IfaceCommon) {
    cancel(common);
    *common.ipv6_autoconf.lock() = DeviceState::default();
}

pub(super) fn carrier_ready(iface: &dyn Iface) -> bool {
    matches!(
        iface.operstate(),
        Operstate::IF_OPER_UP | Operstate::IF_OPER_UNKNOWN
    )
}

pub(super) fn address_from_mac(iface: &dyn Iface, prefix: Ipv6Address) -> Option<Ipv6Address> {
    let mac = iface.mac().0;
    if mac[0] & 1 != 0 || mac == [0; 6] {
        return None;
    }
    let mut address = prefix.octets();
    address[8..].copy_from_slice(&[
        mac[0] ^ 2,
        mac[1],
        mac[2],
        0xff,
        0xfe,
        mac[3],
        mac[4],
        mac[5],
    ]);
    Some(Ipv6Address::from(address))
}

pub(crate) fn poll(netns: &Arc<NetNamespace>) -> Option<i64> {
    let rtnl = rtnl::lock();
    let mut next = None;
    let mut cursor = 0;
    loop {
        let iface = netns
            .device_list()
            .range(cursor..)
            .next()
            .map(|(&index, iface)| (index, iface.clone()));
        let Some((index, iface)) = iface else {
            break;
        };
        cursor = index + 1;
        let now = Instant::now().total_micros();
        let lifetime_due = ipv6_lifetime::poll(&rtnl, netns, &iface, now);
        let (dad_due, solicitation_due) = if iface.type_() == InterfaceType::ETHER {
            (
                poll_device(&rtnl, netns, &iface, now),
                solicitation::poll(netns, &iface),
            )
        } else {
            (None, None)
        };
        if let Some(due) = lifetime_due
            .into_iter()
            .chain(dad_due)
            .chain(solicitation_due)
            .min()
        {
            next = Some(next.map_or(due, |current: i64| current.min(due)));
        }
    }
    next
}

fn poll_device(
    rtnl: &rtnl::RtnlGuard,
    netns: &Arc<NetNamespace>,
    iface: &Arc<dyn Iface>,
    now: i64,
) -> Option<i64> {
    if !iface.flags().contains(InterfaceFlags::UP)
        || !iface.common().ipv6_enabled()
        || iface.mtu() < super::IPV6_MIN_MTU
    {
        return None;
    }
    let carrier = carrier_ready(iface.as_ref());
    let common = iface.common();
    let (start, accept_dad, dad_ready) = {
        let mut state = common.ipv6_autoconf.lock();
        state.dad_ready |= carrier;
        let start = carrier
            && state.mode == 0
            && !state.ready_seen
            && state.retry.is_none_or(|due| due <= now);
        (start, state.accept_dad, state.dad_ready)
    };
    if start {
        let result = start_address(netns, iface, now);
        let mut state = common.ipv6_autoconf.lock();
        state.retry = if matches!(result, Err(SystemError::ENOMEM | SystemError::ENOBUFS)) {
            Some(now.saturating_add(RETRY_US))
        } else {
            state.ready_seen = true;
            None
        };
    }

    let skip = iface.flags().contains(InterfaceFlags::NOARP);
    let mut index = 0;
    loop {
        let probe = common.ipv6_autoconf.lock().probes.get(index).copied();
        let Some(probe) = probe else { break };
        if !probe.can_progress(carrier, dad_ready, skip) {
            index += 1;
            continue;
        }
        // A write to accept_dad during an active probe only changes the
        // interface-wide policy for a detected link-local conflict.
        if probe.conflict
            && accept_dad == 2
            && generated_link_local(iface.as_ref()) == Some(probe.target())
        {
            return super::set_device_ipv6_disabled(rtnl, netns, iface, 1)
                .err()
                .map(|_| now.saturating_add(RETRY_US));
        }
        if probe.conflict || skip || (probe.remaining == 0 && probe.due <= now) {
            match finish_address(rtnl, netns, iface, index, probe) {
                Ok(true) => continue,
                Ok(false) => {}
                Err(_) => return Some(now.saturating_add(RETRY_US)),
            }
        } else if probe.due <= now {
            // DAD counts attempts. A local routing, queue or firewall
            // failure must not leave the address tentative indefinitely.
            let _ = send_probe(netns, iface.as_ref(), probe);
            let mut state = common.ipv6_autoconf.lock();
            let current = &mut state.probes[index];
            current.remaining -= 1;
            current.due = Instant::now().total_micros().saturating_add(RETRANS_US);
        }
        index += 1;
    }
    let state = common.ipv6_autoconf.lock();
    let retry = if carrier { state.retry } else { None };
    retry
        .into_iter()
        .chain(
            state
                .probes
                .iter()
                .filter(|probe| probe.can_progress(carrier, dad_ready, skip))
                .map(|probe| if probe.conflict { now } else { probe.due }),
        )
        .min()
}

fn generated_link_local(iface: &dyn Iface) -> Option<Ipv6Address> {
    address_from_mac(iface, Ipv6Address::new(0xfe80, 0, 0, 0, 0, 0, 0, 0))
}

fn start_address(
    netns: &Arc<NetNamespace>,
    iface: &Arc<dyn Iface>,
    now: i64,
) -> Result<(), SystemError> {
    let Some(target) = generated_link_local(iface.as_ref()) else {
        return Ok(());
    };
    if super::iface_has_address(iface, target.into())
        || iface
            .common()
            .address_metadata()
            .lock()
            .iter()
            .any(|entry| {
                entry.cidr.address() == target.into()
                    || matches!(entry.state, AddressState::AutoLinkLocal(_))
            })
    {
        return Ok(());
    }

    start_address_dad(netns, iface, Ipv6Cidr::new(target, 64), None, now)
}

pub(super) fn start_address_dad(
    netns: &Arc<NetNamespace>,
    iface: &Arc<dyn Iface>,
    address: Ipv6Cidr,
    lifetimes: Option<ipv6_lifetime::Lifetimes>,
    now: i64,
) -> Result<(), SystemError> {
    let cidr = IpCidr::Ipv6(address);
    let (accept_dad, transmits) = {
        let state = iface.common().ipv6_autoconf.lock();
        (state.accept_dad, state.dad_transmits)
    };
    let no_dad = accept_dad <= 0 || transmits == 0 || iface.flags().contains(InterfaceFlags::NOARP);
    let state = if no_dad {
        DadState::Usable
    } else {
        DadState::Tentative
    };
    let address_state = if lifetimes.is_some() {
        AddressState::Slaac(state)
    } else {
        AddressState::AutoLinkLocal(state)
    };
    let prepared = super::prepare_automatic_address(netns, iface, cidr, address_state, lifetimes)?;
    if lifetimes.is_some_and(|lifetimes| lifetimes.expired(Instant::now().total_micros())) {
        return Ok(());
    }
    if !no_dad {
        start_probe(netns, iface.common(), address, transmits as u32, now, false)?;
    }
    publish_address(netns, iface, cidr, state, prepared);
    Ok(())
}

pub(super) fn manual_dad_transmits(iface: &dyn Iface) -> Option<u32> {
    if iface.type_() != InterfaceType::ETHER || iface.flags().contains(InterfaceFlags::NOARP) {
        return None;
    }
    let state = iface.common().ipv6_autoconf.lock();
    (state.accept_dad > 0).then_some(state.dad_transmits as u32)
}

pub(super) fn start_manual_probe(
    netns: &Arc<NetNamespace>,
    common: &IfaceCommon,
    address: Ipv6Cidr,
    transmits: u32,
) -> Result<(), SystemError> {
    start_probe(
        netns,
        common,
        address,
        transmits,
        Instant::now().total_micros(),
        true,
    )
}

fn start_probe(
    netns: &Arc<NetNamespace>,
    common: &IfaceCommon,
    address: Ipv6Cidr,
    transmits: u32,
    now: i64,
    manual: bool,
) -> Result<(), SystemError> {
    common
        .ipv6_autoconf
        .lock()
        .probes
        .try_reserve(1)
        .map_err(|_| SystemError::ENOMEM)?;
    common.multicast_join_ref(netns, address.address().solicited_node().into())?;
    let mut nonce = [0; 6];
    while nonce == [0; 6] {
        crate::libs::rand::fill_random_bytes(&mut nonce);
    }
    // Spread first probes when several addresses become ready together.
    let jitter =
        i64::from(u32::from_be_bytes([nonce[0], nonce[1], nonce[2], nonce[3]])) % RETRANS_US;
    common.ipv6_autoconf.lock().probes.push(Probe {
        address,
        manual,
        nonce,
        due: now.saturating_add(jitter),
        remaining: transmits,
        conflict: false,
        failed: false,
    });
    Ok(())
}

fn finish_address(
    rtnl: &rtnl::RtnlGuard,
    netns: &Arc<NetNamespace>,
    iface: &Arc<dyn Iface>,
    index: usize,
    observed: Probe,
) -> Result<bool, SystemError> {
    let cidr = IpCidr::Ipv6(observed.address);
    let state = if observed.conflict {
        DadState::DadFailed
    } else {
        DadState::Usable
    };
    let (old_state, lifetimes, permanent) = iface
        .common()
        .address_metadata()
        .lock()
        .iter()
        .find(|entry| entry.cidr == cidr)
        .map(|entry| (entry.state, entry.lifetimes, entry.ipv6_permanent))
        .ok_or(SystemError::EADDRNOTAVAIL)?;
    let address_state = match old_state {
        AddressState::Slaac(_) => {
            if observed.conflict
                || lifetimes
                    .is_some_and(|lifetimes| lifetimes.expired(Instant::now().total_micros()))
            {
                ipv6_lifetime::remove(rtnl, netns, iface, cidr)?;
                return Ok(true);
            }
            AddressState::Slaac(state)
        }
        AddressState::ManualDad(_) if observed.conflict && !permanent => {
            ipv6_lifetime::remove(rtnl, netns, iface, cidr)?;
            return Ok(true);
        }
        AddressState::ManualDad(_) if state == DadState::Usable => AddressState::Configured,
        AddressState::ManualDad(_) => AddressState::ManualDad(state),
        _ => AddressState::AutoLinkLocal(state),
    };
    let prepared = super::prepare_automatic_address(netns, iface, cidr, address_state, lifetimes)?;
    if lifetimes.is_some_and(|lifetimes| lifetimes.expired(Instant::now().total_micros())) {
        ipv6_lifetime::remove(rtnl, netns, iface, cidr)?;
        return Ok(true);
    }
    {
        let mut work = iface.common().ipv6_autoconf.lock();
        if work.probes.get(index) != Some(&observed) {
            // A conflict arriving during transaction preparation wins.
            return Ok(false);
        }
        if state == DadState::DadFailed {
            work.probes[index].failed = true;
        } else {
            work.probes.remove(index);
        }
    }
    publish_address(netns, iface, cidr, state, prepared);
    if state == DadState::Usable {
        let _ = iface
            .common()
            .multicast_leave_ref(netns, observed.target().solicited_node().into());
    }
    Ok(true)
}

fn publish_address(
    netns: &Arc<NetNamespace>,
    iface: &Arc<dyn Iface>,
    cidr: IpCidr,
    state: DadState,
    prepared: crate::net::route::PreparedAddressRouteCommit,
) {
    let routes = prepared.publish(netns, iface);
    crate::net::socket::netlink::notify_iface_protocol_change(
        netns,
        iface,
        alloc::vec::Vec::new(),
        (state != DadState::Tentative).then_some(cidr),
        Some(routes),
        alloc::vec::Vec::new(),
    );
}

fn send_probe(
    netns: &Arc<NetNamespace>,
    iface: &dyn Iface,
    probe: Probe,
) -> Result<(), SystemError> {
    send_ndisc(
        netns,
        iface,
        Ipv6Address::UNSPECIFIED,
        probe.target().solicited_node(),
        NdiscRepr::NeighborSolicit {
            target_addr: probe.target(),
            lladdr: None,
            nonce: Some(probe.nonce),
        },
        || {},
    )
}

pub(super) fn send_ndisc(
    netns: &Arc<NetNamespace>,
    iface: &dyn Iface,
    src: Ipv6Address,
    dst: Ipv6Address,
    message: NdiscRepr<'_>,
    before_enqueue: impl FnOnce(),
) -> Result<(), SystemError> {
    let message = Icmpv6Repr::Ndisc(message);
    let ip = Ipv6Repr {
        src_addr: src,
        dst_addr: dst,
        next_header: IpProtocol::Icmpv6,
        hop_limit: 255,
        payload_len: message.buffer_len(),
    };
    let mut packet = reserve_prepared_ip_output(
        iface,
        netns,
        ip.buffer_len() + message.buffer_len(),
        IpVersion::Ipv6,
    )?;
    ip.emit(&mut Ipv6Packet::new_unchecked(packet.bytes_mut()));
    message.emit(
        &src,
        &dst,
        &mut Icmpv6Packet::new_unchecked(&mut packet.bytes_mut()[ip.buffer_len()..]),
        &ChecksumCapabilities::default(),
    );
    let oif = iface.nic_id() as u32;
    submit_prepared_ipv6_before_enqueue(
        netns,
        packet,
        OutputRouteDecision {
            oif,
            required_oif: Some(oif),
            next_hop: dst.into(),
            ip_mtu: iface.mtu(),
            kind: RTN_MULTICAST,
            loopback_anycast: false,
            explicit_ipv6_local: false,
            table: RT_TABLE_LOCAL,
        },
        None,
        before_enqueue,
    )
}

pub(crate) fn receive(
    common: &IfaceCommon,
    netns: &NetNamespace,
    bytes: &[u8],
    offset: usize,
    ifindex: u32,
) {
    if !matches!(bytes.get(offset), Some(135 | 136)) {
        return;
    }
    if !common
        .ipv6_autoconf
        .lock()
        .probes
        .iter()
        .any(|probe| !probe.failed)
    {
        return;
    }
    let Ok(ip) = Ipv6Packet::new_checked(bytes) else {
        return;
    };
    if ip.hop_limit() != 255
        || !matches!(
            fragment_offset(bytes, DefragDomain::local_input(0, ifindex)),
            Ok(None)
        )
    {
        return;
    }
    let Some(payload) = offset
        .checked_sub(40)
        .and_then(|offset| ip.payload().get(offset..))
    else {
        return;
    };
    let Ok(message) = Icmpv6Packet::new_checked(payload) else {
        return;
    };
    let target = message.target_addr();
    let Some(probe) = common
        .ipv6_autoconf
        .lock()
        .probes
        .iter()
        .find(|probe| probe.target() == target && !probe.failed)
        .copied()
    else {
        return;
    };
    let Ok(Icmpv6Repr::Ndisc(ndp)) = Icmpv6Repr::parse(
        &ip.src_addr(),
        &ip.dst_addr(),
        &message,
        &ChecksumCapabilities::default(),
    ) else {
        return;
    };
    let conflict = match ndp {
        NdiscRepr::NeighborSolicit {
            target_addr,
            lladdr,
            nonce,
        } => {
            target_addr == probe.target()
                && ip.src_addr().is_unspecified()
                && ip.dst_addr() == target_addr.solicited_node()
                && lladdr.is_none()
                && nonce != Some(probe.nonce)
        }
        NdiscRepr::NeighborAdvert {
            target_addr, flags, ..
        } => {
            target_addr == probe.target()
                && ip.src_addr().x_is_unicast()
                && (!ip.dst_addr().is_multicast() || !flags.contains(NdiscNeighborFlags::SOLICITED))
        }
        _ => false,
    };
    if conflict {
        let mut state = common.ipv6_autoconf.lock();
        if let Some(current) = state.probes.iter_mut().find(|current| {
            current.target() == probe.target()
                && current.nonce == probe.nonce
                && !current.conflict
                && !current.failed
        }) {
            current.conflict = true;
            drop(state);
            netns.notify_ipv6_autoconf();
        }
    }
}
