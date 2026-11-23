//! Namespace-poller IPv6 link-local configuration. RTNL serializes address
//! changes. The receive path only latches a conflict against a live probe.

use alloc::sync::Arc;
use smoltcp::phy::ChecksumCapabilities;
use smoltcp::wire::{
    Icmpv6Packet, Icmpv6Repr, IpCidr, IpProtocol, IpVersion, Ipv6Address, Ipv6AddressExt,
    Ipv6Packet, Ipv6Repr, NdiscNeighborFlags, NdiscRepr,
};
use system_error::SystemError;

use crate::driver::net::{
    local_output::reserve_prepared_ip_output,
    types::{InterfaceFlags, InterfaceType},
    Iface, IfaceCommon, LinkLocalState, Operstate,
};
use crate::net::{
    ipv6_defrag::{fragment_offset, DefragDomain},
    output::submit_prepared_ipv6_with_related,
    route::{OutputRouteDecision, RTN_MULTICAST, RT_TABLE_LOCAL},
    rtnl,
};
use crate::process::namespace::net_namespace::NetNamespace;
use crate::time::Instant;

use super::ipv6_router_solicitation as solicitation;

const RETRANS_US: i64 = 1_000_000;
const RETRY_US: i64 = 100_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Probe {
    target: Ipv6Address,
    nonce: [u8; 6],
    due: i64,
    remaining: u32,
    conflict: bool,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum Setting {
    AddrGenMode,
    AcceptDad,
    DadTransmits,
    RouterSolicitations,
}

#[derive(Debug)]
pub(crate) struct DeviceState {
    pub(super) solicitation: solicitation::State,
    mode: i32,
    accept_dad: i32,
    dad_transmits: i32,
    ready_seen: bool,
    retry: Option<i64>,
    probe: Option<Probe>,
    group: Option<Ipv6Address>,
}

impl Default for DeviceState {
    fn default() -> Self {
        Self {
            solicitation: solicitation::State::default(),
            mode: 0,
            accept_dad: 1,
            dad_transmits: 1,
            ready_seen: false,
            retry: None,
            probe: None,
            group: None,
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

pub(crate) fn cancel(common: &IfaceCommon, restart: bool) {
    let group = {
        let mut state = common.ipv6_autoconf.lock();
        state.probe = None;
        state.retry = None;
        if restart {
            state.ready_seen = false;
            state.solicitation.link_reset();
        }
        state.group.take()
    };
    if let Some(netns) = common.net_namespace() {
        if let Some(group) = group {
            let _ = common.multicast_leave_ref(&netns, group.into());
        }
        netns.notify_ipv6_autoconf();
    }
}

/// Moving a device discards its IPv6 configuration in the source namespace.
/// Release the pending solicited-node membership before changing ownership.
pub(crate) fn reset_for_netns_move(common: &IfaceCommon) {
    cancel(common, true);
    *common.ipv6_autoconf.lock() = DeviceState::default();
}

pub(super) fn carrier_ready(iface: &dyn Iface) -> bool {
    matches!(
        iface.operstate(),
        Operstate::IF_OPER_UP | Operstate::IF_OPER_UNKNOWN
    )
}

fn link_local(iface: &dyn Iface) -> Option<Ipv6Address> {
    let mac = iface.mac().0;
    if mac[0] & 1 != 0 || mac == [0; 6] {
        return None;
    }
    Some(Ipv6Address::from([
        0xfe,
        0x80,
        0,
        0,
        0,
        0,
        0,
        0,
        mac[0] ^ 2,
        mac[1],
        mac[2],
        0xff,
        0xfe,
        mac[3],
        mac[4],
        mac[5],
    ]))
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
        if iface.type_() != InterfaceType::ETHER {
            continue;
        }
        let dad_due = poll_device(&rtnl, netns, &iface, Instant::now().total_micros());
        let solicitation_due = solicitation::poll(netns, &iface);
        if let Some(due) = dad_due.into_iter().chain(solicitation_due).min() {
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
    let (start, probe, accept_dad) = {
        let state = common.ipv6_autoconf.lock();
        let start = carrier
            && state.mode == 0
            && !state.ready_seen
            && state.retry.is_none_or(|due| due <= now);
        (start, state.probe, state.accept_dad)
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
        return state.retry.or(state.probe.map(|probe| probe.due));
    }

    if let Some(probe) = probe {
        // accept_dad controls whether a new probe starts. A write during
        // an active probe only changes the policy for a detected conflict.
        let skip = iface.flags().contains(InterfaceFlags::NOARP);
        if probe.conflict || skip || (probe.remaining == 0 && probe.due <= now) {
            let result = if probe.conflict && accept_dad == 2 {
                super::set_device_ipv6_disabled(rtnl, netns, iface, 1)
            } else {
                finish_address(netns, iface, probe)
            };
            if result.is_err() {
                return Some(now.saturating_add(RETRY_US));
            }
        } else if probe.due <= now {
            // DAD counts attempts. A local routing, queue or firewall
            // failure must not leave the address tentative indefinitely.
            let _ = send_probe(netns, iface.as_ref(), probe);
            if let Some(current) = common.ipv6_autoconf.lock().probe.as_mut() {
                current.remaining -= 1;
                current.due = Instant::now().total_micros().saturating_add(RETRANS_US);
            }
        }
    }
    let state = common.ipv6_autoconf.lock();
    let retry = if carrier { state.retry } else { None };
    retry.or(state
        .probe
        .map(|probe| if probe.conflict { now } else { probe.due }))
}

fn start_address(
    netns: &Arc<NetNamespace>,
    iface: &Arc<dyn Iface>,
    now: i64,
) -> Result<(), SystemError> {
    let Some(target) = link_local(iface.as_ref()) else {
        return Ok(());
    };
    let cidr = IpCidr::new(target.into(), 64);
    if super::iface_has_address(iface, target.into())
        || super::has_address_object(iface, cidr)
        || iface
            .common()
            .address_metadata()
            .lock()
            .iter()
            .any(|entry| {
                matches!(
                    entry.state,
                    crate::driver::net::AddressState::AutoLinkLocal(_)
                )
            })
    {
        return Ok(());
    }

    let (accept_dad, transmits) = {
        let state = iface.common().ipv6_autoconf.lock();
        (state.accept_dad, state.dad_transmits)
    };
    let no_dad = accept_dad <= 0 || transmits == 0 || iface.flags().contains(InterfaceFlags::NOARP);
    let state = if no_dad {
        LinkLocalState::Usable
    } else {
        LinkLocalState::Tentative
    };
    let prepared = super::prepare_automatic_link_local(netns, iface, cidr, state)?;
    if !no_dad {
        let group = target.solicited_node();
        iface.common().multicast_join_ref(netns, group.into())?;
        let mut nonce = [0; 6];
        while nonce == [0; 6] {
            crate::libs::rand::fill_random_bytes(&mut nonce);
        }
        // Spread first probes when several ports become ready together.
        let jitter =
            i64::from(u32::from_be_bytes([nonce[0], nonce[1], nonce[2], nonce[3]])) % RETRANS_US;
        let mut work = iface.common().ipv6_autoconf.lock();
        work.group = Some(group);
        work.probe = Some(Probe {
            target,
            nonce,
            due: now.saturating_add(jitter),
            remaining: transmits as u32,
            conflict: false,
        });
    }
    publish_address(netns, iface, cidr, state, prepared);
    Ok(())
}

fn finish_address(
    netns: &Arc<NetNamespace>,
    iface: &Arc<dyn Iface>,
    observed: Probe,
) -> Result<(), SystemError> {
    let cidr = IpCidr::new(observed.target.into(), 64);
    let state = if observed.conflict {
        LinkLocalState::DadFailed
    } else {
        LinkLocalState::Usable
    };
    let prepared = super::prepare_automatic_link_local(netns, iface, cidr, state)?;
    let group = {
        let mut work = iface.common().ipv6_autoconf.lock();
        if work.probe != Some(observed) {
            // A conflict arriving during transaction preparation wins.
            return Ok(());
        }
        work.probe = None;
        if state == LinkLocalState::Usable {
            work.group.take()
        } else {
            None
        }
    };
    publish_address(netns, iface, cidr, state, prepared);
    if let Some(group) = group {
        let _ = iface.common().multicast_leave_ref(netns, group.into());
    }
    Ok(())
}

fn publish_address(
    netns: &Arc<NetNamespace>,
    iface: &Arc<dyn Iface>,
    cidr: IpCidr,
    state: LinkLocalState,
    prepared: crate::net::route::PreparedAddressRouteCommit,
) {
    let routes = prepared.publish(netns, iface);
    crate::net::socket::netlink::notify_iface_protocol_change(
        netns,
        iface,
        alloc::vec::Vec::new(),
        (state != LinkLocalState::Tentative).then_some(cidr),
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
        probe.target.solicited_node(),
        NdiscRepr::NeighborSolicit {
            target_addr: probe.target,
            lladdr: None,
            nonce: Some(probe.nonce),
        },
    )
}

pub(super) fn send_ndisc(
    netns: &Arc<NetNamespace>,
    iface: &dyn Iface,
    src: Ipv6Address,
    dst: Ipv6Address,
    message: NdiscRepr<'_>,
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
    submit_prepared_ipv6_with_related(
        netns,
        packet,
        OutputRouteDecision {
            oif,
            required_oif: Some(oif),
            next_hop: dst.into(),
            ip_mtu: iface.mtu(),
            kind: RTN_MULTICAST,
            table: RT_TABLE_LOCAL,
        },
        None,
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
    let Some(probe) = common.ipv6_autoconf.lock().probe else {
        return;
    };
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
            target_addr == probe.target
                && ip.src_addr().is_unspecified()
                && ip.dst_addr() == target_addr.solicited_node()
                && lladdr.is_none()
                && nonce != Some(probe.nonce)
        }
        NdiscRepr::NeighborAdvert {
            target_addr, flags, ..
        } => {
            target_addr == probe.target
                && ip.src_addr().x_is_unicast()
                && (!ip.dst_addr().is_multicast() || !flags.contains(NdiscNeighborFlags::SOLICITED))
        }
        _ => false,
    };
    if conflict {
        let mut state = common.ipv6_autoconf.lock();
        if let Some(current) = state.probe.as_mut() {
            if current.target == probe.target && current.nonce == probe.nonce && !current.conflict {
                current.conflict = true;
                drop(state);
                netns.notify_ipv6_autoconf();
            }
        }
    }
}
