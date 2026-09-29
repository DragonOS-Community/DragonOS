mod create;
mod query;

pub(super) use create::do_new_link;
pub(super) use query::do_get_link;
use query::iface_to_link_message;

use crate::{
    driver::net::{
        bridge::BridgeIface,
        types::{InterfaceFlags, InterfaceType},
        veth::VethInterface,
        Iface,
    },
    net::link::{LinkFlagsUpdate, LinkMtuUpdate, LinkMutationCommit, LinkTarget, LinkUpdate},
    net::socket::{
        netlink::{
            message::segment::{
                header::{CMsgSegHdr, GetRequestFlags, NewRequestFlags, SegHdrCommonFlags},
                CSegmentType,
            },
            route::{
                kern::utils::{
                    finish_response, kernel_notify_header, multicast_notify, RTMGRP_LINK,
                },
                message::{
                    attr::link::{append_nested_link_attr, parse_nested_link_attrs, LinkAttr},
                    segment::{
                        link::{CIfinfoMsg, LinkMessageFlags, LinkSegment, LinkSegmentBody},
                        RouteNlSegment,
                    },
                },
            },
        },
        AddressFamily,
    },
    process::namespace::net_namespace::NetNamespace,
};
use alloc::{ffi::CString, string::String, sync::Arc, vec::Vec};
use core::num::NonZero;
use smoltcp::wire::EthernetAddress;
use system_error::SystemError;

use super::RtnlRequestContext;

pub(crate) fn notify_link_change(iface: &Arc<dyn Iface>) {
    let Some(netns) = iface.net_namespace() else {
        return;
    };
    let segment = iface_to_link_message(
        &kernel_notify_header(CSegmentType::NEWLINK),
        CSegmentType::NEWLINK,
        iface,
    );
    match segment {
        Ok(segment) => multicast_notify(netns, RTMGRP_LINK, RouteNlSegment::NewLink(segment)),
        Err(err) => log::warn!(
            "netlink route: failed to build link notification: {:?}",
            err
        ),
    }
}

/// A DELLINK message must be captured while the device and its peer are
/// still registered. This also serves peer removal caused by netns exit.
pub(crate) struct PreparedLinkDelete(RouteNlSegment);

pub(crate) fn prepare_link_delete(
    iface: &Arc<dyn Iface>,
) -> Result<PreparedLinkDelete, SystemError> {
    let segment = iface_to_link_message(
        &kernel_notify_header(CSegmentType::DELLINK),
        CSegmentType::DELLINK,
        iface,
    )?;
    Ok(PreparedLinkDelete(RouteNlSegment::DelLink(segment)))
}

pub(crate) fn notify_link_delete(netns: Arc<NetNamespace>, message: PreparedLinkDelete) {
    multicast_notify(netns, RTMGRP_LINK, message.0);
}

fn nla_name(payload: &[u8]) -> Result<&str, SystemError> {
    let end = payload
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(payload.len());
    let name = core::str::from_utf8(&payload[..end]).map_err(|_| SystemError::EINVAL)?;
    if name.is_empty()
        || name.len() >= 16
        || name == "."
        || name == ".."
        || name.bytes().any(|byte| {
            byte == b'/'
                || byte == b':'
                || matches!(byte, b' ' | b'\t' | b'\n' | b'\x0b' | b'\x0c' | b'\r')
        })
    {
        return Err(SystemError::EINVAL);
    }
    Ok(name)
}

fn linkinfo(request: &LinkSegment) -> Result<(&str, Option<&[u8]>), SystemError> {
    let data = request
        .attrs()
        .iter()
        .rev()
        .find_map(|attr| match attr {
            LinkAttr::LinkInfo(data) => Some(data.as_slice()),
            _ => None,
        })
        .ok_or(SystemError::EOPNOTSUPP_OR_ENOTSUP)?;
    let mut kind = None;
    let mut info_data = None;
    for attr in parse_nested_link_attrs(data)? {
        match attr.kind {
            1 => kind = Some(nla_name(attr.payload)?),
            2 => info_data = Some(attr.payload),
            _ => return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP),
        }
    }
    Ok((kind.ok_or(SystemError::EINVAL)?, info_data))
}

fn link_mac(bytes: &[u8]) -> Result<EthernetAddress, SystemError> {
    if bytes.len() != 6 || bytes[0] & 1 != 0 || bytes.iter().all(|byte| *byte == 0) {
        return Err(SystemError::EINVAL);
    }
    let mut address = [0u8; 6];
    address.copy_from_slice(bytes);
    Ok(EthernetAddress(address))
}

fn target_namespace(
    context: &RtnlRequestContext,
    fd: Option<u32>,
    pid: Option<u32>,
) -> Result<Arc<NetNamespace>, SystemError> {
    match (fd, pid) {
        (Some(_), Some(_)) => Err(SystemError::EINVAL),
        (Some(fd), None) => context.netns_from_fd(fd),
        (None, Some(pid)) => context.netns_from_pid(pid),
        (None, None) => Ok(context.netns()),
    }
}

pub(super) fn do_del_link(
    rtnl: &crate::net::rtnl::RtnlGuard,
    request_segment: &LinkSegment,
    netns: Arc<NetNamespace>,
) -> Result<Vec<RouteNlSegment>, SystemError> {
    let iface = find_iface_for_link(request_segment, &netns)?;
    if iface.type_() == InterfaceType::LOOPBACK {
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }
    let bridge_carriers = BridgeCarrierSnapshot::related_to(&iface);
    let deleted = prepare_link_delete(&iface)?;
    let peer = iface
        .as_any_ref()
        .downcast_ref::<VethInterface>()
        .and_then(VethInterface::peer_veth_opt)
        .and_then(|peer| {
            let peer_netns = peer.net_namespace()?;
            let peer_iface: Arc<dyn Iface> = peer;
            Some((peer_netns, peer_iface))
        });
    let peer_deleted = peer
        .as_ref()
        .map(|(_, peer)| prepare_link_delete(peer))
        .transpose()?;
    crate::net::link::topology::delete_link(rtnl, &netns, iface)?;
    notify_link_delete(netns, deleted);
    if let (Some((peer_netns, _)), Some(peer_deleted)) = (peer, peer_deleted) {
        notify_link_delete(peer_netns, peer_deleted);
    }
    BridgeCarrierSnapshot::notify_related(bridge_carriers);
    Ok(Vec::new())
}

fn find_iface_for_link(
    request_segment: &LinkSegment,
    netns: &Arc<NetNamespace>,
) -> Result<Arc<dyn Iface>, SystemError> {
    if let Some(index) = request_segment.body().index {
        return netns
            .device_list()
            .get(&(index.get() as usize))
            .cloned()
            .ok_or(SystemError::ENODEV);
    }
    let name = request_segment.attrs().iter().find_map(|attr| match attr {
        LinkAttr::Name(name) => name.to_str().ok(),
        _ => None,
    });
    let name = name.ok_or(SystemError::EINVAL)?;
    netns
        .device_list()
        .values()
        .find(|iface| iface.common().with_iface_name(|current| current == name))
        .cloned()
        .ok_or(SystemError::ENODEV)
}

/// A veth admin-state change also changes the peer's carrier. The peer may
/// live in a different netns, so its linkwatch notification is independent of
/// the RTM_NEWLINK sent for the device named in the request.
struct PeerCarrierSnapshot {
    peer: Arc<VethInterface>,
    had_carrier: bool,
}

/// Bridge carrier follows its ports, but the bridge's RTM_NEWLINK is a
/// separate notification from the changed veth's notification. Capture the
/// before-state outside driver locks and publish only an actual transition.
struct BridgeCarrierSnapshot {
    bridge: Arc<BridgeIface>,
    had_carrier: bool,
}

impl BridgeCarrierSnapshot {
    fn capture(bridge: Arc<BridgeIface>) -> Self {
        let had_carrier = !bridge
            .net_state()
            .contains(crate::driver::net::NetDeivceState::__LINK_STATE_NOCARRIER);
        Self {
            bridge,
            had_carrier,
        }
    }

    fn related_to(iface: &Arc<dyn Iface>) -> [Option<Self>; 2] {
        let Some(veth) = iface.as_any_ref().downcast_ref::<VethInterface>() else {
            return [None, None];
        };
        let first = veth.bridge_master();
        let second = veth.peer_veth_opt().and_then(|peer| peer.bridge_master());
        let second = second.filter(|bridge| {
            first
                .as_ref()
                .is_none_or(|first| !Arc::ptr_eq(first, bridge))
        });
        [first.map(Self::capture), second.map(Self::capture)]
    }

    fn notify_if_changed(self) {
        let has_carrier = !self
            .bridge
            .net_state()
            .contains(crate::driver::net::NetDeivceState::__LINK_STATE_NOCARRIER);
        if self.had_carrier != has_carrier {
            let iface: Arc<dyn Iface> = self.bridge;
            notify_link_change(&iface);
        }
    }

    fn notify_related(bridges: [Option<Self>; 2]) {
        for bridge in bridges.into_iter().flatten() {
            bridge.notify_if_changed();
        }
    }
}

impl PeerCarrierSnapshot {
    fn capture(iface: &Arc<dyn Iface>) -> Option<Self> {
        let peer = iface
            .as_any_ref()
            .downcast_ref::<VethInterface>()?
            .peer_veth_opt()?;
        let had_carrier = !peer
            .net_state()
            .contains(crate::driver::net::NetDeivceState::__LINK_STATE_NOCARRIER);
        Some(Self { peer, had_carrier })
    }

    fn notify_if_changed(self) {
        let has_carrier = !self
            .peer
            .net_state()
            .contains(crate::driver::net::NetDeivceState::__LINK_STATE_NOCARRIER);
        if self.had_carrier != has_carrier {
            let iface: Arc<dyn Iface> = self.peer;
            notify_link_change(&iface);
        }
    }
}

pub(super) fn do_set_link(
    rtnl: &crate::net::rtnl::RtnlGuard,
    request_segment: &LinkSegment,
    context: &RtnlRequestContext,
) -> Result<Vec<RouteNlSegment>, SystemError> {
    let source_netns = context.netns();
    if request_segment
        .attrs()
        .iter()
        .any(|attr| matches!(attr, LinkAttr::LinkInfo(_)))
    {
        let iface = find_iface_for_link(request_segment, &source_netns)?;
        let (kind, data) = linkinfo(request_segment)?;
        let matches_kind = match kind {
            "bridge" => iface.as_any_ref().is::<BridgeIface>(),
            "veth" => iface.as_any_ref().is::<VethInterface>(),
            _ => false,
        };
        if !matches_kind || data.is_some_and(|data| !data.is_empty()) {
            return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
        }
    }
    let netns_fd = request_segment
        .attrs()
        .iter()
        .rev()
        .find_map(|attr| match attr {
            LinkAttr::NetNsFd(fd) => Some(*fd),
            _ => None,
        });
    let netns_pid = request_segment
        .attrs()
        .iter()
        .rev()
        .find_map(|attr| match attr {
            LinkAttr::NetNsPid(pid) => Some(*pid),
            _ => None,
        });
    let master_index = request_segment
        .attrs()
        .iter()
        .rev()
        .find_map(|attr| match attr {
            LinkAttr::Master(index) => Some(*index),
            _ => None,
        });
    let requested_mac = request_segment
        .attrs()
        .iter()
        .rev()
        .find_map(|attr| match attr {
            LinkAttr::Address(bytes) => Some(link_mac(bytes)),
            _ => None,
        })
        .transpose()?;
    let target_netns = target_namespace(context, netns_fd, netns_pid)?;
    let (target, mut update) = parse_setlink_request(request_segment)?;
    let mut moved_iface = None;
    if (netns_fd.is_some() || netns_pid.is_some()) && !Arc::ptr_eq(&source_netns, &target_netns) {
        let iface = find_iface_for_link(request_segment, &source_netns)?;
        let peer_carrier = PeerCarrierSnapshot::capture(&iface);
        let bridge_carriers = BridgeCarrierSnapshot::related_to(&iface);
        let old_link = iface_to_link_message(
            &kernel_notify_header(CSegmentType::DELLINK),
            CSegmentType::DELLINK,
            &iface,
        )?;
        crate::net::link::topology::move_veth(
            rtnl,
            &source_netns,
            &target_netns,
            iface.clone(),
            update.new_name.as_deref(),
        )?;
        multicast_notify(
            source_netns.clone(),
            RTMGRP_LINK,
            RouteNlSegment::DelLink(old_link),
        );
        notify_link_change(&iface);
        if let Some(peer_carrier) = peer_carrier {
            peer_carrier.notify_if_changed();
        }
        BridgeCarrierSnapshot::notify_related(bridge_carriers);
        update.new_name = None;
        moved_iface = Some(iface);
    }
    let target = moved_iface
        .as_ref()
        .map(|iface| LinkTarget::Index(iface.nic_id() as u32))
        .unwrap_or(target);
    let peer_carrier = moved_iface
        .clone()
        .or_else(|| find_iface_for_link(request_segment, &target_netns).ok())
        .as_ref()
        .and_then(PeerCarrierSnapshot::capture);
    let bridge_carriers = moved_iface
        .clone()
        .or_else(|| find_iface_for_link(request_segment, &target_netns).ok())
        .as_ref()
        .map(BridgeCarrierSnapshot::related_to)
        .unwrap_or([None, None]);
    let mut mac_changed = false;
    if let Some(mac) = requested_mac {
        let iface = moved_iface
            .clone()
            .or_else(|| find_iface_for_link(request_segment, &target_netns).ok())
            .ok_or(SystemError::ENODEV)?;
        if let Some(bridge) = iface.as_any_ref().downcast_ref::<BridgeIface>() {
            bridge.set_mac(mac)?;
        } else if let Some(veth) = iface.as_any_ref().downcast_ref::<VethInterface>() {
            veth.set_mac(mac)?;
        } else {
            return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
        }
        mac_changed = true;
    }
    let committed = match crate::net::link::mutate_link(rtnl, &target_netns, target, update) {
        Ok(committed) => committed,
        Err(error) => {
            if mac_changed {
                if let Some(iface) =
                    moved_iface.or_else(|| find_iface_for_link(request_segment, &target_netns).ok())
                {
                    notify_link_change(&iface);
                }
            }
            return Err(error);
        }
    };
    let iface = committed.iface.clone();
    let notify_mac = mac_changed && committed.changes.is_empty();
    notify_link_commit(&target_netns, committed);
    if let Some(peer_carrier) = peer_carrier {
        peer_carrier.notify_if_changed();
    }
    BridgeCarrierSnapshot::notify_related(bridge_carriers);
    if notify_mac {
        notify_link_change(&iface);
    }
    if let Some(master_index) = master_index {
        let master = if master_index == 0 {
            None
        } else {
            Some(
                target_netns
                    .device_list()
                    .get(&(master_index as usize))
                    .cloned()
                    .ok_or(SystemError::EINVAL)?,
            )
        };
        let old_bridge = BridgeCarrierSnapshot::related_to(&iface);
        let new_bridge = master
            .as_ref()
            .and_then(|master| crate::net::link::topology::as_bridge(master.clone()))
            .map(BridgeCarrierSnapshot::capture);
        crate::net::link::topology::set_master(rtnl, &target_netns, iface.clone(), master)?;
        notify_link_change(&iface);
        BridgeCarrierSnapshot::notify_related(old_bridge);
        if let Some(new_bridge) = new_bridge {
            new_bridge.notify_if_changed();
        }
    }
    Ok(Vec::new())
}

pub(crate) fn notify_link_commit(netns: &Arc<NetNamespace>, committed: LinkMutationCommit) {
    let LinkMutationCommit {
        iface,
        changes,
        renamed_ipv4,
        removed_addresses,
        route_changes,
        removed_neighbors,
        rename_old_devpath,
        related,
    } = committed;
    if let Some(old_devpath) = rename_old_devpath {
        crate::driver::net::sysfs::netdev_emit_move_uevent(iface.clone(), old_devpath);
    }
    if !changes.is_empty() {
        notify_link_change(&iface);
    }
    for removed in removed_addresses {
        super::addr::notify_removed_address(
            netns.clone(),
            &iface,
            removed.cidr,
            &removed.label,
            removed.broadcast,
        );
    }
    for cidr in renamed_ipv4 {
        super::addr::notify_address_change(netns.clone(), &iface, cidr);
    }
    if let Some(changes) = route_changes {
        notify_link_route_changes(netns, changes);
    }
    for entry in removed_neighbors {
        super::neigh::notify_removed_entry(netns, entry);
    }
    if let Some(related) = related {
        notify_link_commit(netns, *related);
    }
}

fn notify_link_route_changes(
    netns: &Arc<NetNamespace>,
    changes: crate::net::route::RouteNotifications,
) {
    // Linux 6.6 withdraws IPv4 aliases silently through fib_flush(), while
    // fib6_ifdown emits RTM_DELROUTE. Link-up address-derived routes are regular
    // insertions and emit RTM_NEWROUTE for both families.
    let crate::net::route::RouteNotifications { added, removed } = changes;
    for route in removed {
        super::route::notify_route(netns, CSegmentType::DELROUTE, route);
    }
    for route in added {
        super::route::notify_route(netns, CSegmentType::NEWROUTE, route);
    }
}

fn parse_setlink_request(
    request_segment: &LinkSegment,
) -> Result<(LinkTarget<'_>, LinkUpdate), SystemError> {
    if request_segment.body().pad.is_some() {
        return Err(SystemError::EINVAL);
    }
    // nla_parse() keeps the last attribute of a given type.
    let requested_name = request_segment
        .attrs()
        .iter()
        .filter_map(|attr| {
            if let LinkAttr::Name(name) = attr {
                name.to_str().ok()
            } else {
                None
            }
        })
        .next_back();
    if let Some(index) = request_segment.body().index {
        let mut update = LinkUpdate::default();
        if let Some(name) = requested_name {
            update.new_name = Some(try_string_from_str(name)?);
        }
        parse_setlink_attrs(request_segment, &mut update, true)?;
        update.flags = parse_flags(request_segment);
        return Ok((LinkTarget::Index(index.get()), update));
    }
    let name = requested_name.ok_or(SystemError::EINVAL)?;
    let mut update = LinkUpdate::default();
    parse_setlink_attrs(request_segment, &mut update, false)?;
    update.flags = parse_flags(request_segment);
    Ok((LinkTarget::Name(name), update))
}

fn parse_setlink_attrs(
    request_segment: &LinkSegment,
    update: &mut LinkUpdate,
    name_is_mutation: bool,
) -> Result<(), SystemError> {
    for attr in request_segment.attrs() {
        match attr {
            LinkAttr::Name(name) => {
                name.to_str().map_err(|_| SystemError::EINVAL)?;
                if name_is_mutation {
                    // Already copied above; keep parsing focused on policy.
                }
            }
            LinkAttr::Mtu(mtu) => update.mtu = Some(LinkMtuUpdate::Rtnetlink(*mtu)),
            LinkAttr::TxqLen(len) => update.tx_queue_len = Some(*len),
            LinkAttr::Master(_)
            | LinkAttr::NetNsFd(_)
            | LinkAttr::NetNsPid(_)
            | LinkAttr::Address(_) => {}
            LinkAttr::LinkInfo(_) => {
                let (_, data) = linkinfo(request_segment)?;
                if data.is_some_and(|data| !data.is_empty()) {
                    return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
                }
            }
            LinkAttr::NewIfIndex(_) => return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP),
            LinkAttr::Allmulti(_) => return Err(SystemError::EINVAL),
            LinkAttr::Promiscuity(_) | LinkAttr::LinkMode(_) | LinkAttr::ExtMask(_) => {}
            _ => return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP),
        }
    }
    Ok(())
}

fn parse_flags(request_segment: &LinkSegment) -> Option<LinkFlagsUpdate> {
    let body = request_segment.body();
    if body.flags.is_empty() && body.change.is_empty() {
        return None;
    }
    Some(LinkFlagsUpdate::Masked {
        requested: InterfaceFlags::from_bits_truncate(body.flags.bits()),
        change: InterfaceFlags::from_bits_truncate(body.change.bits()),
    })
}

fn try_string_from_str(source: &str) -> Result<String, SystemError> {
    let mut result = String::new();
    result
        .try_reserve_exact(source.len())
        .map_err(|_| SystemError::ENOMEM)?;
    result.push_str(source);
    Ok(result)
}
