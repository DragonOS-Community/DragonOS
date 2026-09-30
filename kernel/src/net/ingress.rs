//! One receive-side policy entry shared by direct and NAPI smoltcp polling.
//! Route handoff is added here only after the packet has passed IP validation.

use super::{
    conntrack::{
        parse_ipv4_conntrack, parse_ipv6_conntrack, CtAddress, CtChecksumMode, CtNatRequest,
        CtPacketContext, NatManipSide,
    },
    ipv6_defrag::{fragment_offset as ipv6_fragment_offset, DefragDomain as Ipv6DefragDomain},
    nftables::{
        NftDeviceNames, NftIpv4Hook, NftNatAction, NftNatEvent, NftNatPacket, NftNatProgress,
        NftPacket, RulesetSnapshot,
    },
    route::{OutputRouteGuard, RTN_BROADCAST, RTN_LOCAL, RTN_UNICAST},
    socket::inet::raw::{RawIngressListener, RawIngressWork},
};
use crate::driver::net::{
    inject_owned_local_ip_packet_if_epoch, types::InterfaceFlags, veth::VethInterface, Iface,
    LocalPacketOrigin,
};
use crate::process::namespace::net_namespace::{
    DefragSource, NetNamespace, PendingIpFragment, PendingIpv4Fragment, PendingIpv6Fragment,
};
use alloc::{sync::Arc, vec::Vec};
use core::cell::{Cell, RefCell};
use smoltcp::{
    iface::{
        IngressPacket, IpIngressFilter, LocalInputVerdict, PreRoutingVerdict, RouteInputVerdict,
        RoutedIngressPacket,
    },
    phy::PacketMeta,
    wire::{
        ipv6::AddressExt, EthernetAddress, HardwareAddress, Icmpv6Message, IpAddress, IpProtocol,
        IpVersion, Ipv4Packet, Ipv6ExtHeader, Ipv6Packet,
    },
};
use system_error::SystemError;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum IngressStage {
    #[default]
    Pending,
    PreRoutingDone,
    LocalOutput,
    LocalInputDone,
}

/// The stage cell is private to one synchronous smoltcp poll. A local RX
/// token changes it only while its bytes are being consumed; a physical RX
/// token leaves it at Pending. No packet-level state is stored globally.
pub(crate) struct NetIngressFilter<'a> {
    ruleset: &'a Arc<RulesetSnapshot>,
    netns: &'a Arc<NetNamespace>,
    raw_listeners: &'a [RawIngressListener],
    owner_ifindex: u32,
    device_names: &'a NftDeviceNames,
    stage: &'a Cell<IngressStage>,
    /// Packet-owned route classification installed only by a local RX token.
    handoff_broadcast: &'a Cell<bool>,
    /// Set only after this packet's ingress FIB selected this interface.
    local_route_selected: Cell<bool>,
    broadcast_route_selected: Cell<bool>,
    mark: &'a Cell<u32>,
    forward_fragment: &'a Cell<Option<super::forward_mtu::ReassembledForwardInfo>>,
    packet_context: &'a RefCell<Option<CtPacketContext>>,
    routes: Option<&'a OutputRouteGuard<'a>>,
    fib_routes: Option<&'a OutputRouteGuard<'a>>,
    work: &'a mut Vec<RoutedIngressWork>,
}

/// Named poll inputs keep the two direct/NAPI call sites in sync as receive
/// policy grows, without adding state to an interface or a network namespace.
pub(crate) struct NetIngressFilterInit<'a> {
    pub(crate) ruleset: &'a Arc<RulesetSnapshot>,
    pub(crate) netns: &'a Arc<NetNamespace>,
    pub(crate) raw_listeners: &'a [RawIngressListener],
    pub(crate) owner_ifindex: u32,
    pub(crate) device_names: &'a NftDeviceNames,
    pub(crate) stage: &'a Cell<IngressStage>,
    pub(crate) handoff_broadcast: &'a Cell<bool>,
    pub(crate) mark: &'a Cell<u32>,
    pub(crate) forward_fragment: &'a Cell<Option<super::forward_mtu::ReassembledForwardInfo>>,
    pub(crate) packet_context: &'a RefCell<Option<CtPacketContext>>,
    pub(crate) routes: Option<&'a OutputRouteGuard<'a>>,
    pub(crate) fib_routes: Option<&'a OutputRouteGuard<'a>>,
    pub(crate) work: &'a mut Vec<RoutedIngressWork>,
}

/// A route decision and owned datagram are transferred out of the receive
/// callback together. No target interface is entered while the source's
/// smoltcp, socket-set, or FIB locks are held.
pub(crate) enum RoutedIngressWork {
    Raw(RawIngressWork),
    Defrag {
        netns: Arc<NetNamespace>,
        fragment: PendingIpFragment,
    },
    Local {
        target: Arc<dyn Iface>,
        target_epoch: u64,
        ingress_ifindex: u32,
        source_mac: EthernetAddress,
        broadcast: bool,
        packet: Vec<u8>,
        ct_context: CtPacketContext,
        mark: u32,
    },
    Forward {
        target: Arc<dyn Iface>,
        target_epoch: u64,
        next_hop: IpAddress,
        packet: Vec<u8>,
        fragment_info: Option<super::forward_mtu::ReassembledForwardInfo>,
        netns: Arc<NetNamespace>,
        ct_context: CtPacketContext,
    },
    MtuErrorIpv4 {
        netns: Arc<NetNamespace>,
        packet: Vec<u8>,
        mtu: usize,
        ct_context: Option<CtPacketContext>,
    },
    MtuErrorIpv6 {
        netns: Arc<NetNamespace>,
        packet: Vec<u8>,
        mtu: usize,
        ct_context: Option<CtPacketContext>,
    },
}

impl RoutedIngressWork {
    pub(crate) fn execute(self) {
        let result = match self {
            Self::Raw(work) => {
                work.execute();
                return;
            }
            Self::Defrag { netns, fragment } => {
                // The source interface, socket set, and FIB locks have already
                // been released. A full ring rejects and drops this owned
                // fragment outside its short spinlock critical section.
                // Local IPv6 delivery uses the same bounded worker after
                // LOCAL_IN. CT rule commits prepare it before publishing
                // hooks; otherwise allocate lazily outside interface locks.
                if matches!(fragment, PendingIpFragment::Ipv6(_))
                    && !netns.ipv6_defrag_enabled()
                    && (netns.prepare_ipv6_defrag().is_err() || netns.enable_ipv6_defrag().is_err())
                {
                    return;
                }
                let _ = netns.queue_ip_fragment(fragment);
                return;
            }
            Self::MtuErrorIpv4 {
                netns,
                packet,
                mtu,
                ct_context,
            } => {
                if let Err(error) = super::forward_mtu::send_ipv4_frag_needed(
                    &netns,
                    &packet,
                    mtu,
                    ct_context.as_ref(),
                ) {
                    log::debug!("forward MTU feedback discarded: {:?}", error);
                }
                return;
            }
            Self::MtuErrorIpv6 {
                netns,
                packet,
                mtu,
                ct_context,
            } => {
                if let Err(error) = super::forward_mtu::send_ipv6_packet_too_big(
                    &netns,
                    &packet,
                    mtu,
                    ct_context.as_ref(),
                ) {
                    log::debug!("forward MTU feedback discarded: {:?}", error);
                }
                return;
            }
            Self::Local {
                target,
                target_epoch,
                ingress_ifindex,
                source_mac,
                broadcast,
                packet,
                ct_context,
                mark,
            } => inject_owned_local_ip_packet_if_epoch(
                target.as_ref(),
                ingress_ifindex,
                source_mac,
                packet,
                broadcast,
                LocalPacketOrigin::LinkIngressPreRouted,
                Some(ct_context),
                mark,
                Some(target_epoch),
            ),
            Self::Forward {
                target,
                target_epoch,
                next_hop,
                packet,
                fragment_info,
                netns,
                ct_context,
            } => {
                // A forward handoff runs after the source poll releases its
                // locks. Pin veth TX admission until this handoff finishes so
                // a netns move cannot retarget an already-routed packet.
                let _move_guard = if target.as_any_ref().is::<VethInterface>() {
                    let Some(guard) = target.common().try_acquire_tx() else {
                        return;
                    };
                    Some(guard)
                } else {
                    None
                };
                if target.common().namespace_epoch() != target_epoch {
                    return;
                }
                if !target.flags().contains(InterfaceFlags::UP) {
                    return;
                }
                if !matches!(
                    (IpVersion::of_packet(packet.as_slice()), next_hop),
                    (Ok(IpVersion::Ipv4), IpAddress::Ipv4(_))
                        | (Ok(IpVersion::Ipv6), IpAddress::Ipv6(_))
                ) {
                    return;
                }
                let feedback =
                    match super::forward_mtu::ForwardMtuFeedback::new(netns, ct_context, &packet) {
                        Ok(feedback) => feedback,
                        Err(_) => return,
                    };
                let result = target.route_and_send_or_queue_with_fragment(
                    &next_hop,
                    packet.as_slice(),
                    fragment_info,
                    feedback.as_ref(),
                );
                if result == Err(SystemError::EMSGSIZE) {
                    if let Some(feedback) = feedback {
                        let _ = feedback.send(&packet, target.mtu());
                    }
                }
                result
            }
        };
        if let Err(error) = result {
            log::debug!("routed ingress packet discarded: {:?}", error);
        }
    }
}

impl<'a> NetIngressFilter<'a> {
    pub(crate) fn new(init: NetIngressFilterInit<'a>) -> Self {
        let NetIngressFilterInit {
            ruleset,
            netns,
            raw_listeners,
            owner_ifindex,
            device_names,
            stage,
            handoff_broadcast,
            mark,
            forward_fragment,
            packet_context,
            routes,
            fib_routes,
            work,
        } = init;
        Self {
            ruleset,
            netns,
            raw_listeners,
            owner_ifindex,
            device_names,
            stage,
            handoff_broadcast,
            local_route_selected: Cell::new(false),
            broadcast_route_selected: Cell::new(false),
            mark,
            forward_fragment,
            packet_context,
            routes,
            fib_routes,
            work,
        }
    }

    fn ct_active_for(&self, version: IpVersion) -> bool {
        // Runtime resources may be activated immediately before publishing a
        // new ruleset. In-flight packets pinned to the old snapshot must
        // retain that snapshot's untracked semantics during the handover.
        self.ruleset.conntrack_registered(version)
            && self.netns.conntrack().is_active_family(version)
    }

    fn pre_routed_context_valid(&self, version: IpVersion) -> bool {
        !self.ct_active_for(version) || self.packet_context.borrow().is_some()
    }

    fn take_handoff_context(&self, version: IpVersion) -> Option<CtPacketContext> {
        if self.ct_active_for(version) {
            // A tracked PRE_ROUTING packet must carry its identity across
            // interface owners; otherwise it could evade later CT/NAT hooks.
            self.packet_context.borrow_mut().take()
        } else {
            // A packet admitted under an older ruleset remains untracked
            // even if CT is activated before its deferred handoff runs.
            self.packet_context.borrow_mut().take();
            Some(CtPacketContext::Untracked)
        }
    }

    /// Classify at the fixed -200 step in the compiled hook program. Local
    /// reinjection retains OUTPUT's context instead of creating a second one.
    fn classify_ingress(&self, version: IpVersion, bytes: &[u8]) -> bool {
        if !self.ct_active_for(version) {
            return true;
        }
        if self.stage.get() == IngressStage::LocalOutput {
            return self.packet_context.borrow().is_some();
        }
        let parsed = match version {
            IpVersion::Ipv4 => parse_ipv4_conntrack(bytes),
            IpVersion::Ipv6 => parse_ipv6_conntrack(bytes),
        };
        let context = self
            .netns
            .conntrack()
            .classify(parsed, crate::time::Instant::now());
        self.packet_context.borrow_mut().replace(context);
        true
    }

    /// A packet rejected by a later filter never publishes its private flow.
    /// Only INPUT or POSTROUTING may confirm a new candidate. Return the
    /// confirmed identity so a deferred forwarding error stays RELATED.
    fn confirm_accepted_packet_context(&self, version: IpVersion) -> Option<CtPacketContext> {
        if !self.ct_active_for(version) {
            return Some(CtPacketContext::Untracked);
        }
        let context = self.packet_context.borrow_mut().take()?;
        match context {
            CtPacketContext::Candidate(candidate) => {
                let confirmed = self
                    .netns
                    .confirm_conntrack(candidate, crate::time::Instant::now())
                    .ok()?;
                let flow = match confirmed {
                    super::conntrack::CtConfirm::Inserted(flow)
                    | super::conntrack::CtConfirm::Reused(flow) => flow,
                };
                Some(CtPacketContext::Matched(super::conntrack::CtMatch {
                    flow,
                    direction: super::conntrack::CtDirection::Original,
                    state: super::conntrack::CtPacketState::New,
                }))
            }
            other => Some(other),
        }
    }

    fn confirm_accepted_packet(&self, version: IpVersion) -> bool {
        self.confirm_accepted_packet_context(version).is_some()
    }

    /// The fixed outer NAT event runs at -100/+100 inside the ruleset's
    /// priority-ordered hook program. The selected first-packet mapping stays
    /// private until INPUT/POSTROUTING confirmation. Packet bytes change only
    /// at Finish, so a failed rule never leaves a partially translated skb.
    fn nat_event(
        &self,
        bytes: &mut [u8],
        event: NftNatEvent,
        oif: u32,
        next_hop: Option<IpAddress>,
        active_side: &Cell<Option<NatManipSide>>,
    ) -> Result<NftNatProgress, SystemError> {
        let mode = if self.stage.get() == IngressStage::LocalOutput {
            CtChecksumMode::Skip
        } else {
            CtChecksumMode::Verify
        };
        match event {
            NftNatEvent::Begin(side) => {
                let context = self.packet_context.borrow();
                let context = context.as_ref().ok_or(SystemError::EINVAL)?;
                // The loopback receive copy has already traversed OUTPUT and
                // POSTROUTING with this same CT identity. PRE/IN filter chains
                // still run, but neither direction may bind or rewrite again.
                if self.stage.get() == IngressStage::LocalOutput {
                    return Ok(NftNatProgress::SkipRules);
                }
                match context {
                    CtPacketContext::Candidate(candidate) if !candidate.nat_initialized(side) => {
                        active_side.set(Some(side));
                        Ok(NftNatProgress::Continue)
                    }
                    CtPacketContext::Candidate(candidate) => {
                        candidate
                            .nat_rewrite(side)
                            .apply(bytes, mode)
                            .map_err(|_| SystemError::EINVAL)?;
                        Ok(NftNatProgress::SkipRules)
                    }
                    _ => {
                        context
                            .rewrite_confirmed_nat(bytes, side, mode)
                            .map_err(|_| SystemError::EINVAL)?;
                        Ok(NftNatProgress::SkipRules)
                    }
                }
            }
            NftNatEvent::Rule(action) => {
                let expected_side = active_side.get().ok_or(SystemError::EINVAL)?;
                let mut context = self.packet_context.borrow_mut();
                let Some(CtPacketContext::Candidate(candidate)) = context.as_mut() else {
                    return Err(SystemError::EINVAL);
                };
                let (side, request) = match action {
                    NftNatAction::Dnat(request) => (NatManipSide::Destination, request),
                    NftNatAction::Snat(request) => (NatManipSide::Source, request),
                    NftNatAction::Masquerade { ports } => {
                        let destination = match IpVersion::of_packet(bytes) {
                            Ok(IpVersion::Ipv4) => Ipv4Packet::new_checked(&*bytes)
                                .map(|packet| IpAddress::Ipv4(packet.dst_addr())),
                            Ok(IpVersion::Ipv6) => Ipv6Packet::new_checked(&*bytes)
                                .map(|packet| IpAddress::Ipv6(packet.dst_addr())),
                            _ => return Err(SystemError::EINVAL),
                        }
                        .map_err(|_| SystemError::EINVAL)?;
                        let next_hop = next_hop.ok_or(SystemError::EINVAL)?;
                        let address = self
                            .fib_routes
                            .and_then(|routes| {
                                routes.masquerade_address(oif, destination, next_hop)
                            })
                            .ok_or(SystemError::EINVAL)?;
                        let address = match address {
                            IpAddress::Ipv4(address) => CtAddress::V4(address.octets()),
                            IpAddress::Ipv6(address) => CtAddress::V6(address.octets()),
                        };
                        let request = CtNatRequest::masquerade(address, oif, ports)
                            .map_err(|_| SystemError::EINVAL)?;
                        (NatManipSide::Source, request)
                    }
                };
                if side != expected_side {
                    return Err(SystemError::EINVAL);
                }
                self.netns
                    .conntrack()
                    .select_nat_mapping(candidate, side, request, crate::time::Instant::now())
                    .map_err(|_| SystemError::EINVAL)?;
                Ok(NftNatProgress::SkipRules)
            }
            NftNatEvent::Finish(side) => {
                match active_side.replace(None) {
                    None => return Ok(NftNatProgress::Continue),
                    Some(active) if active == side => {}
                    Some(_) => return Err(SystemError::EINVAL),
                }
                let mut context = self.packet_context.borrow_mut();
                if let Some(CtPacketContext::Candidate(candidate)) = context.as_mut() {
                    if !candidate.nat_initialized(side) {
                        candidate
                            .initialize_null_binding(side)
                            .map_err(|_| SystemError::EINVAL)?;
                    }
                    candidate
                        .nat_rewrite(side)
                        .apply(bytes, mode)
                        .map_err(|_| SystemError::EINVAL)?;
                }
                Ok(NftNatProgress::Continue)
            }
        }
    }

    fn allows_ipv4(&self, hook: NftIpv4Hook, bytes: &[u8], iif: u32, oif: u32) -> bool {
        if !(self.ruleset.has_ipv4_hook(hook)
            || self.ct_active_for(IpVersion::Ipv4) && hook == NftIpv4Hook::PreRouting)
        {
            return true;
        }
        if self.ruleset.ipv4_hook_requires_route_lookup(hook) && self.fib_routes.is_none() {
            // A route-aware rule must never run with an unprotected FIB view.
            return false;
        }
        let lookup = |address| {
            self.fib_routes
                .map_or(crate::net::route::RTN_UNICAST, |routes| {
                    routes.ipv4_addr_type(address)
                })
        };
        let (iifname, oifname) = if self.ruleset.hook_requires_iface_names(hook) {
            let (Some(iifname), Some(oifname)) =
                (self.device_names.get(iif), self.device_names.get(oif))
            else {
                return false;
            };
            (iifname, oifname)
        } else {
            ([0; 16], [0; 16])
        };
        let packet = NftPacket::new(bytes, &lookup)
            .with_interface_names(iifname, oifname)
            .with_conntrack_cell(self.packet_context)
            .with_mark(self.mark);
        self.ruleset
            .allows_ipv4_hook_with_tracking(hook, &packet, || {
                hook != NftIpv4Hook::PreRouting || self.classify_ingress(IpVersion::Ipv4, bytes)
            })
    }

    fn allows_ipv6(&self, hook: NftIpv4Hook, bytes: &[u8], iif: u32, oif: u32) -> bool {
        if !(self.ruleset.has_ipv6_hook(hook)
            || self.ct_active_for(IpVersion::Ipv6) && hook == NftIpv4Hook::PreRouting)
        {
            return true;
        }
        if self.ruleset.ipv6_hook_requires_local_destination(hook) && self.fib_routes.is_none() {
            return false;
        }
        let is_local = |address| {
            self.fib_routes.is_some_and(|routes| {
                routes
                    .lookup(IpAddress::Ipv6(address), None)
                    .is_some_and(|decision| decision.kind == RTN_LOCAL)
            })
        };
        let (iifname, oifname) = if self.ruleset.ipv6_hook_requires_iface_names(hook) {
            let (Some(iifname), Some(oifname)) =
                (self.device_names.get(iif), self.device_names.get(oif))
            else {
                return false;
            };
            (iifname, oifname)
        } else {
            ([0; 16], [0; 16])
        };
        let packet = NftPacket::new_ipv6(bytes)
            .with_ipv6_local_destination(&is_local)
            .with_interface_names(iifname, oifname)
            .with_conntrack_cell(self.packet_context)
            .with_mark(self.mark);
        self.ruleset
            .allows_ipv6_hook_with_tracking(hook, &packet, || {
                hook != NftIpv4Hook::PreRouting || self.classify_ingress(IpVersion::Ipv6, bytes)
            })
    }

    fn allows_ipv4_nat(
        &self,
        hook: NftIpv4Hook,
        bytes: &mut [u8],
        iif: u32,
        oif: u32,
        next_hop: Option<IpAddress>,
    ) -> bool {
        if self.ruleset.ipv4_hook_requires_route_lookup(hook) && self.fib_routes.is_none() {
            return false;
        }
        let lookup = |address| {
            self.fib_routes
                .map_or(crate::net::route::RTN_UNICAST, |routes| {
                    routes.ipv4_addr_type(address)
                })
        };
        let (iifname, oifname) = if self.ruleset.hook_requires_iface_names(hook) {
            let (Some(iifname), Some(oifname)) =
                (self.device_names.get(iif), self.device_names.get(oif))
            else {
                return false;
            };
            (iifname, oifname)
        } else {
            ([0; 16], [0; 16])
        };
        let active_side = Cell::new(None);
        let redirect = |bytes: &[u8]| {
            let destination = Ipv4Packet::new_checked(bytes).ok()?.dst_addr();
            let IpAddress::Ipv4(address) = self
                .fib_routes?
                .redirect_address(iif, IpAddress::Ipv4(destination))?
            else {
                return None;
            };
            Some(CtAddress::V4(address.octets()))
        };
        self.ruleset
            .evaluate_ipv4_hook_with_nat(
                hook,
                NftNatPacket::new_ipv4(bytes, self.packet_context, iifname, oifname, &lookup)
                    .with_mark(self.mark)
                    .with_redirect_address(&redirect),
                |bytes| {
                    hook != NftIpv4Hook::PreRouting || self.classify_ingress(IpVersion::Ipv4, bytes)
                },
                |bytes, event| self.nat_event(bytes, event, oif, next_hop, &active_side),
            )
            .unwrap_or(false)
    }

    fn allows_ipv6_nat(
        &self,
        hook: NftIpv4Hook,
        bytes: &mut [u8],
        iif: u32,
        oif: u32,
        next_hop: Option<IpAddress>,
    ) -> bool {
        if self.ruleset.ipv6_hook_requires_local_destination(hook) && self.fib_routes.is_none() {
            return false;
        }
        let is_local = |address| {
            self.fib_routes.is_some_and(|routes| {
                routes
                    .lookup(IpAddress::Ipv6(address), None)
                    .is_some_and(|decision| decision.kind == RTN_LOCAL)
            })
        };
        let (iifname, oifname) = if self.ruleset.ipv6_hook_requires_iface_names(hook) {
            let (Some(iifname), Some(oifname)) =
                (self.device_names.get(iif), self.device_names.get(oif))
            else {
                return false;
            };
            (iifname, oifname)
        } else {
            ([0; 16], [0; 16])
        };
        let active_side = Cell::new(None);
        let redirect = |bytes: &[u8]| {
            let destination = Ipv6Packet::new_checked(bytes).ok()?.dst_addr();
            let IpAddress::Ipv6(address) = self
                .fib_routes?
                .redirect_address(iif, IpAddress::Ipv6(destination))?
            else {
                return None;
            };
            Some(CtAddress::V6(address.octets()))
        };
        self.ruleset
            .evaluate_ipv6_hook_with_nat(
                hook,
                NftNatPacket::new_ipv6(bytes, self.packet_context, iifname, oifname)
                    .with_ipv6_local_destination(&is_local)
                    .with_mark(self.mark)
                    .with_redirect_address(&redirect),
                |bytes| {
                    hook != NftIpv4Hook::PreRouting || self.classify_ingress(IpVersion::Ipv6, bytes)
                },
                |bytes, event| self.nat_event(bytes, event, oif, next_hop, &active_side),
            )
            .unwrap_or(false)
    }

    fn route_ipv6(
        &mut self,
        packet: &mut RoutedIngressPacket<'_, '_>,
        meta: PacketMeta,
        source_hardware_addr: HardwareAddress,
    ) -> RouteInputVerdict {
        if !self.netns.ipv6_forwarding_enabled() && self.routes.is_none() {
            return RouteInputVerdict::Pass;
        }
        let Some(routes) = self.routes else {
            // A routing-enabled namespace must never deliver a transit packet
            // locally merely because its protected FIB snapshot was absent.
            return RouteInputVerdict::Drop;
        };
        let ingress_ifindex = if meta.id != 0 {
            meta.id
        } else {
            self.owner_ifindex
        };
        let Ok(ipv6) = Ipv6Packet::new_checked(packet.bytes()) else {
            return RouteInputVerdict::Drop;
        };
        let source = ipv6.src_addr();
        let destination = ipv6.dst_addr();
        if destination.is_multicast() {
            return RouteInputVerdict::Pass;
        }
        let Some(route) = routes.lookup_ingress(destination.into(), ingress_ifindex) else {
            return RouteInputVerdict::Pass;
        };
        match route.matched.kind {
            // IPv6 link-local unicast is scoped to its ingress link. A local
            // address on another interface must not be weak-host delivered.
            RTN_LOCAL if destination.is_link_local() && route.oif != ingress_ifindex => {
                RouteInputVerdict::Drop
            }
            // Neighbor discovery is link control, not a transport datagram.
            // A weak-host global destination can be owned by another iface,
            // but Linux still updates the neighbor table of the actual
            // ingress device. Keep the packet on that device's receive path.
            RTN_LOCAL if route.oif != ingress_ifindex && is_ipv6_ndisc(&ipv6) => {
                RouteInputVerdict::NeighborDiscovery
            }
            RTN_LOCAL if route.oif == self.owner_ifindex => RouteInputVerdict::Pass,
            RTN_LOCAL => {
                let HardwareAddress::Ethernet(source_mac) = source_hardware_addr else {
                    return RouteInputVerdict::Drop;
                };
                let Some(target) = routes.ingress_device(route.oif) else {
                    return RouteInputVerdict::Drop;
                };
                if self.work.try_reserve(1).is_err() {
                    return RouteInputVerdict::Drop;
                }
                let Some(ct_context) = self.take_handoff_context(IpVersion::Ipv6) else {
                    return RouteInputVerdict::Drop;
                };
                let Ok(packet) = packet.take_owned() else {
                    return RouteInputVerdict::Drop;
                };
                self.work.push(RoutedIngressWork::Local {
                    target_epoch: target.common().namespace_epoch(),
                    target,
                    ingress_ifindex,
                    source_mac,
                    broadcast: false,
                    packet,
                    ct_context,
                    mark: self.mark.get(),
                });
                RouteInputVerdict::Forward
            }
            RTN_UNICAST => {
                if !self.netns.ipv6_forwarding_enabled()
                    || source.is_unspecified()
                    || source.is_multicast()
                    || source.is_link_local()
                    || source == smoltcp::wire::Ipv6Address::LOCALHOST
                    || destination.is_link_local()
                    || destination == smoltcp::wire::Ipv6Address::LOCALHOST
                    || ipv6.hop_limit() <= 1
                {
                    return RouteInputVerdict::Drop;
                }
                let Some(target) = routes.ingress_device(route.oif) else {
                    return RouteInputVerdict::Drop;
                };
                if !target.flags().contains(InterfaceFlags::UP) || self.work.try_reserve(1).is_err()
                {
                    return RouteInputVerdict::Drop;
                }
                let mtu = target.mtu();
                // IPv6 cannot use an interface whose link MTU is below the
                // protocol minimum.  An explicit route may outlive removal
                // of the interface's IPv6 addresses when its MTU shrinks;
                // do not advertise an undeliverable 1280-byte PMTU for it.
                if mtu < 1280 {
                    return RouteInputVerdict::Drop;
                }
                if packet.bytes().len() > mtu
                    && !self
                        .forward_fragment
                        .get()
                        .is_some_and(|info| info.may_refragment(mtu))
                {
                    let Ok(packet) = packet.take_owned() else {
                        return RouteInputVerdict::Drop;
                    };
                    self.work.push(RoutedIngressWork::MtuErrorIpv6 {
                        netns: self.netns.clone(),
                        packet,
                        mtu,
                        ct_context: self.packet_context.borrow().clone(),
                    });
                    return RouteInputVerdict::Forward;
                }
                let Ok(mut packet) = packet.take_owned() else {
                    return RouteInputVerdict::Drop;
                };
                let Ok(mut forwarded) = Ipv6Packet::new_checked(packet.as_mut_slice()) else {
                    return RouteInputVerdict::Drop;
                };
                forwarded.set_hop_limit(forwarded.hop_limit() - 1);
                let allowed = self.allows_ipv6(
                    NftIpv4Hook::Forward,
                    forwarded.as_ref(),
                    ingress_ifindex,
                    route.oif,
                );
                if !allowed {
                    return RouteInputVerdict::Drop;
                }
                let post_allowed = if self
                    .ruleset
                    .hook_may_nat(IpVersion::Ipv6, NftIpv4Hook::PostRouting)
                {
                    self.allows_ipv6_nat(
                        NftIpv4Hook::PostRouting,
                        packet.as_mut_slice(),
                        0,
                        route.oif,
                        Some(route.next_hop),
                    )
                } else {
                    self.allows_ipv6(NftIpv4Hook::PostRouting, packet.as_slice(), 0, route.oif)
                };
                if !post_allowed {
                    return RouteInputVerdict::Drop;
                }
                let Some(ct_context) = self.confirm_accepted_packet_context(IpVersion::Ipv6) else {
                    return RouteInputVerdict::Drop;
                };
                self.work.push(RoutedIngressWork::Forward {
                    target_epoch: target.common().namespace_epoch(),
                    target,
                    next_hop: route.next_hop,
                    packet,
                    fragment_info: self.forward_fragment.get(),
                    netns: self.netns.clone(),
                    ct_context,
                });
                RouteInputVerdict::Forward
            }
            _ => RouteInputVerdict::Drop,
        }
    }
}

fn is_ipv6_ndisc(packet: &Ipv6Packet<&[u8]>) -> bool {
    let mut protocol = packet.next_header();
    let mut payload = packet.payload();
    if protocol == IpProtocol::HopByHop {
        let Ok(ext) = Ipv6ExtHeader::new_checked(payload) else {
            return false;
        };
        let length = (usize::from(ext.header_len()) + 1) * 8;
        protocol = ext.next_header();
        payload = &payload[length..];
    }
    protocol == IpProtocol::Icmpv6
        && payload
            .first()
            .is_some_and(|kind| Icmpv6Message::from(*kind).is_ndisc())
}

impl IpIngressFilter for NetIngressFilter<'_> {
    fn continue_ingress_poll(&self) -> bool {
        let current = self.ruleset.generation == self.netns.nftables().generation();
        if current {
            // This runs before the next receive token is consumed. A local
            // token then installs its saved mark; a physical token keeps zero.
            // Do not clear in begin_packet: a defrag reinjection still uses
            // IngressStage::Pending and must retain offset zero's mark.
            self.mark.set(0);
            self.forward_fragment.set(None);
        }
        current
    }

    fn packet_mark(&self) -> u32 {
        self.mark.get()
    }

    fn restore_packet_mark(&self, mark: u32) {
        self.mark.set(mark);
    }

    fn begin_packet(&mut self, _meta: PacketMeta) {
        self.local_route_selected.set(false);
        self.broadcast_route_selected.set(false);
        // A physical receive token starts a new packet. A local handoff token
        // has already installed its owned context before this callback runs.
        if self.stage.get() == IngressStage::Pending {
            self.packet_context.borrow_mut().take();
        }
    }

    fn local_route_selected(&self) -> bool {
        self.stage.get() == IngressStage::LocalOutput
            || (self.stage.get() == IngressStage::PreRoutingDone && self.handoff_broadcast.get())
            || self.local_route_selected.get()
    }

    fn broadcast_route_selected(&self) -> bool {
        self.broadcast_route_selected.get()
            || (self.stage.get() != IngressStage::Pending && self.handoff_broadcast.get())
    }

    fn defragment_ipv4(&self) -> bool {
        // The current executable slice only inspects IP-level verdicts. It
        // must not force transit fragments through smoltcp's local assembler.
        false
    }

    fn applies_to(&self, version: IpVersion) -> bool {
        match version {
            IpVersion::Ipv4 => {
                self.handoff_broadcast.get()
                    || self.ct_active_for(IpVersion::Ipv4)
                    || !self.raw_listeners.is_empty()
                    || self.ruleset.has_ipv4_hook(NftIpv4Hook::LocalIn)
                    || match self.stage.get() {
                        IngressStage::Pending => {
                            self.routes.is_some()
                                || self.ruleset.has_ipv4_hook(NftIpv4Hook::PreRouting)
                        }
                        IngressStage::LocalOutput => {
                            // The output route already selected a local socket owner.
                            // Even with no rules, the filtered path must retain that
                            // decision for addresses covered by a loopback subnet.
                            true
                        }
                        IngressStage::PreRoutingDone | IngressStage::LocalInputDone => false,
                    }
            }
            IpVersion::Ipv6 => {
                self.stage.get() == IngressStage::LocalInputDone
                    || self.ct_active_for(IpVersion::Ipv6)
                    || self.netns.ipv6_forwarding_enabled()
                    || !self.raw_listeners.is_empty()
                    || self.ruleset.has_ipv6_hook(NftIpv4Hook::LocalIn)
                    || (self.stage.get() == IngressStage::Pending && self.routes.is_some())
                    || (self.stage.get() != IngressStage::PreRoutingDone
                        && self.ruleset.has_ipv6_hook(NftIpv4Hook::PreRouting))
            }
        }
    }

    fn applies_to_packet(&self, version: IpVersion, packet: &[u8]) -> bool {
        if self.applies_to(version) {
            return true;
        }
        // Fragmented IPv6 local delivery needs the filtered path even with
        // no nft rules or conntrack. Avoid the slower path for ordinary IPv6.
        version == IpVersion::Ipv6
            && packet
                .get(6)
                .is_some_and(|next| matches!(next, 0 | 43 | 44 | 51 | 60))
            && !matches!(
                ipv6_fragment_offset(packet, Ipv6DefragDomain::local_input(0, 0)),
                Ok(None)
            )
    }

    fn pre_routing_ipv6_defrag(
        &mut self,
        packet: &mut IngressPacket<'_>,
        meta: PacketMeta,
        source_hardware_addr: HardwareAddress,
    ) -> PreRoutingVerdict {
        if self.stage.get() == IngressStage::LocalInputDone || !self.ct_active_for(IpVersion::Ipv6)
        {
            return PreRoutingVerdict::Pass;
        }
        let ingress_ifindex = if meta.id != 0 {
            meta.id
        } else {
            self.owner_ifindex
        };
        if ingress_ifindex == 0 {
            return PreRoutingVerdict::Drop;
        }
        let source = if self.stage.get() == IngressStage::LocalOutput {
            DefragSource::LocalOutput
        } else {
            DefragSource::LinkIngress
        };
        let domain = if source == DefragSource::LocalOutput {
            Ipv6DefragDomain::local_out(0, ingress_ifindex)
        } else {
            Ipv6DefragDomain::pre_routing(0, ingress_ifindex)
        };
        let first_fragment = match ipv6_fragment_offset(packet.bytes(), domain) {
            Ok(None) => return PreRoutingVerdict::Pass,
            Ok(Some(offset)) => offset == 0,
            Err(_) => return PreRoutingVerdict::Drop,
        };
        if matches!(
            self.stage.get(),
            IngressStage::PreRoutingDone | IngressStage::LocalInputDone
        ) || self.work.try_reserve(1).is_err()
        {
            return PreRoutingVerdict::Drop;
        }
        let ct_context = if self.ct_active_for(IpVersion::Ipv6)
            && source == DefragSource::LocalOutput
            && first_fragment
        {
            let Some(context) = self.packet_context.borrow_mut().take() else {
                return PreRoutingVerdict::Drop;
            };
            match Arc::try_new(context) {
                Ok(context) => Some(context),
                Err(_) => return PreRoutingVerdict::Drop,
            }
        } else {
            None
        };
        let Ok(bytes) = packet.take_owned() else {
            return PreRoutingVerdict::Drop;
        };
        self.work.push(RoutedIngressWork::Defrag {
            netns: self.netns.clone(),
            fragment: PendingIpFragment::Ipv6(PendingIpv6Fragment {
                packet: bytes,
                ingress_ifindex,
                owner_ifindex: self.owner_ifindex,
                source_hardware_addr,
                source,
                ct_context,
                mark: self.mark.get(),
            }),
        });
        PreRoutingVerdict::Drop
    }

    fn pre_routing(
        &mut self,
        packet: &mut IngressPacket<'_>,
        meta: PacketMeta,
        _source_hardware_addr: HardwareAddress,
    ) -> PreRoutingVerdict {
        let Ok(version) = IpVersion::of_packet(packet.bytes()) else {
            return PreRoutingVerdict::Drop;
        };
        if matches!(
            self.stage.get(),
            IngressStage::PreRoutingDone | IngressStage::LocalInputDone
        ) {
            if !self.pre_routed_context_valid(version) {
                return PreRoutingVerdict::Drop;
            }
            return PreRoutingVerdict::Pass;
        }
        debug_assert!(matches!(
            self.stage.get(),
            IngressStage::Pending | IngressStage::LocalOutput
        ));
        // RX token metadata is trusted; a zero id means a direct device
        // token, whose owner interface is the ingress device.
        let ingress_ifindex = if meta.id != 0 {
            meta.id
        } else {
            self.owner_ifindex
        };
        if ingress_ifindex == 0 {
            return PreRoutingVerdict::Drop;
        }
        match version {
            IpVersion::Ipv4 => {
                let allowed = if self.ruleset.hook_may_nat(version, NftIpv4Hook::PreRouting) {
                    packet.writable().is_ok_and(|bytes| {
                        self.allows_ipv4_nat(
                            NftIpv4Hook::PreRouting,
                            bytes,
                            ingress_ifindex,
                            0,
                            None,
                        )
                    })
                } else {
                    self.allows_ipv4(NftIpv4Hook::PreRouting, packet.bytes(), ingress_ifindex, 0)
                };
                if allowed {
                    PreRoutingVerdict::Pass
                } else {
                    PreRoutingVerdict::Drop
                }
            }
            IpVersion::Ipv6 => {
                let allowed = if self.ruleset.hook_may_nat(version, NftIpv4Hook::PreRouting) {
                    packet.writable().is_ok_and(|bytes| {
                        self.allows_ipv6_nat(
                            NftIpv4Hook::PreRouting,
                            bytes,
                            ingress_ifindex,
                            0,
                            None,
                        )
                    })
                } else {
                    self.allows_ipv6(NftIpv4Hook::PreRouting, packet.bytes(), ingress_ifindex, 0)
                };
                if allowed {
                    PreRoutingVerdict::Pass
                } else {
                    PreRoutingVerdict::Drop
                }
            }
        }
    }

    fn route_input(
        &mut self,
        packet: &mut RoutedIngressPacket<'_, '_>,
        meta: PacketMeta,
        source_hardware_addr: HardwareAddress,
    ) -> RouteInputVerdict {
        if matches!(
            self.stage.get(),
            IngressStage::PreRoutingDone | IngressStage::LocalInputDone
        ) {
            return RouteInputVerdict::Pass;
        }
        // The local-output route already selected this socket owner. Linux
        // receives the packet through lo and runs PRE_ROUTING, but must not
        // choose a second owner from the ingress FIB (notably for broadcast).
        if self.stage.get() == IngressStage::LocalOutput {
            return RouteInputVerdict::Pass;
        }
        if matches!(IpVersion::of_packet(packet.bytes()), Ok(IpVersion::Ipv6)) {
            return self.route_ipv6(packet, meta, source_hardware_addr);
        }
        let Some(routes) = self.routes else {
            return RouteInputVerdict::Pass;
        };
        let ingress_ifindex = if meta.id != 0 {
            meta.id
        } else {
            self.owner_ifindex
        };
        let Ok(ipv4) = Ipv4Packet::new_checked(packet.bytes()) else {
            return RouteInputVerdict::Drop;
        };
        let source = ipv4.src_addr().octets();
        let destination = ipv4.dst_addr();
        // The smoltcp source check has already run. The ingress FIB additionally
        // rejects martian sources that must not arrive from a veth link.
        if ingress_ifindex != crate::net::LOOPBACK_IFINDEX as u32
            && (source[0] == 127 || destination.octets()[0] == 127)
        {
            return RouteInputVerdict::Drop;
        }
        if source[0] == 0 && (source != [0; 4] || destination.octets() != [255; 4]) {
            return RouteInputVerdict::Drop;
        }
        // An explicitly configured broadcast address is not part of
        // smoltcp's CIDR-derived source classification. The local FIB is the
        // authority for that address, so it cannot be accepted as a unicast
        // source merely because its octets have an ordinary host shape.
        if routes
            .lookup_ingress(IpAddress::Ipv4(ipv4.src_addr()), ingress_ifindex)
            .is_some_and(|route| route.matched.kind == RTN_BROADCAST)
        {
            return RouteInputVerdict::Drop;
        }
        // Multicast routing is not configured by this FIB. A default unicast
        // route must never steal IGMP or joined-group UDP from the local stack.
        if destination.is_multicast() {
            return RouteInputVerdict::Pass;
        }
        let Some(route) = routes.lookup_ingress(destination.into(), ingress_ifindex) else {
            return RouteInputVerdict::Pass;
        };
        match route.matched.kind {
            RTN_LOCAL | RTN_BROADCAST if route.oif == self.owner_ifindex => {
                self.local_route_selected.set(true);
                self.broadcast_route_selected
                    .set(route.matched.kind == RTN_BROADCAST);
                RouteInputVerdict::Pass
            }
            RTN_LOCAL | RTN_BROADCAST => {
                let HardwareAddress::Ethernet(source_mac) = source_hardware_addr else {
                    return RouteInputVerdict::Drop;
                };
                let Some(target) = routes.ingress_device(route.oif) else {
                    return RouteInputVerdict::Drop;
                };
                if self.work.try_reserve(1).is_err() {
                    return RouteInputVerdict::Drop;
                }
                let Some(ct_context) = self.take_handoff_context(IpVersion::Ipv4) else {
                    return RouteInputVerdict::Drop;
                };
                let Ok(packet) = packet.take_owned() else {
                    return RouteInputVerdict::Drop;
                };
                self.work.push(RoutedIngressWork::Local {
                    target_epoch: target.common().namespace_epoch(),
                    target,
                    ingress_ifindex,
                    source_mac,
                    broadcast: route.matched.kind == RTN_BROADCAST,
                    packet,
                    ct_context,
                    mark: self.mark.get(),
                });
                RouteInputVerdict::Forward
            }
            RTN_UNICAST => {
                if !self.netns.ipv4_forwarding_enabled() || ipv4.hop_limit() <= 1 {
                    // The forwarding-error path must emit ICMP Time Exceeded
                    // before this can claim full IPv4 forwarding semantics.
                    return RouteInputVerdict::Drop;
                }
                let Some(target) = routes.ingress_device(route.oif) else {
                    return RouteInputVerdict::Drop;
                };
                if !target.flags().contains(InterfaceFlags::UP) || self.work.try_reserve(1).is_err()
                {
                    return RouteInputVerdict::Drop;
                }
                let mtu = target.mtu();
                if packet.bytes().len() > mtu
                    && ipv4.dont_frag()
                    && !self
                        .forward_fragment
                        .get()
                        .is_some_and(|info| info.may_refragment(mtu))
                {
                    let Ok(packet) = packet.take_owned() else {
                        return RouteInputVerdict::Drop;
                    };
                    self.work.push(RoutedIngressWork::MtuErrorIpv4 {
                        netns: self.netns.clone(),
                        packet,
                        mtu,
                        ct_context: self.packet_context.borrow().clone(),
                    });
                    return RouteInputVerdict::Forward;
                }
                let Ok(mut packet) = packet.take_owned() else {
                    return RouteInputVerdict::Drop;
                };
                let Ok(mut forwarded) = Ipv4Packet::new_checked(packet.as_mut_slice()) else {
                    return RouteInputVerdict::Drop;
                };
                forwarded.set_hop_limit(forwarded.hop_limit() - 1);
                forwarded.fill_checksum();
                let allowed = self.allows_ipv4(
                    NftIpv4Hook::Forward,
                    forwarded.as_ref(),
                    ingress_ifindex,
                    route.oif,
                );
                if !allowed {
                    return RouteInputVerdict::Drop;
                }
                let post_allowed = if self
                    .ruleset
                    .hook_may_nat(IpVersion::Ipv4, NftIpv4Hook::PostRouting)
                {
                    self.allows_ipv4_nat(
                        NftIpv4Hook::PostRouting,
                        packet.as_mut_slice(),
                        0,
                        route.oif,
                        Some(route.next_hop),
                    )
                } else {
                    self.allows_ipv4(NftIpv4Hook::PostRouting, packet.as_slice(), 0, route.oif)
                };
                if !post_allowed {
                    return RouteInputVerdict::Drop;
                }
                let Some(ct_context) = self.confirm_accepted_packet_context(IpVersion::Ipv4) else {
                    return RouteInputVerdict::Drop;
                };
                self.work.push(RoutedIngressWork::Forward {
                    target_epoch: target.common().namespace_epoch(),
                    target,
                    next_hop: route.next_hop,
                    packet,
                    fragment_info: self.forward_fragment.get(),
                    netns: self.netns.clone(),
                    ct_context,
                });
                RouteInputVerdict::Forward
            }
            _ => RouteInputVerdict::Drop,
        }
    }

    fn local_input(
        &mut self,
        packet: &mut IngressPacket<'_>,
        meta: PacketMeta,
        source_hardware_addr: HardwareAddress,
        protocol: IpProtocol,
        _transport_offset: usize,
    ) -> LocalInputVerdict {
        let version = match IpVersion::of_packet(packet.bytes()) {
            Ok(version) => version,
            _ => return LocalInputVerdict::Drop,
        };
        let ingress_ifindex = if meta.id != 0 {
            meta.id
        } else {
            self.owner_ifindex
        };
        if ingress_ifindex == 0 {
            return LocalInputVerdict::Drop;
        }
        if self.stage.get() != IngressStage::LocalInputDone {
            let allowed = match version {
                IpVersion::Ipv4 if self.ruleset.hook_may_nat(version, NftIpv4Hook::LocalIn) => {
                    packet.writable().is_ok_and(|bytes| {
                        self.allows_ipv4_nat(NftIpv4Hook::LocalIn, bytes, ingress_ifindex, 0, None)
                    })
                }
                IpVersion::Ipv6 if self.ruleset.hook_may_nat(version, NftIpv4Hook::LocalIn) => {
                    packet.writable().is_ok_and(|bytes| {
                        self.allows_ipv6_nat(NftIpv4Hook::LocalIn, bytes, ingress_ifindex, 0, None)
                    })
                }
                IpVersion::Ipv4 => {
                    self.allows_ipv4(NftIpv4Hook::LocalIn, packet.bytes(), ingress_ifindex, 0)
                }
                IpVersion::Ipv6 => {
                    self.allows_ipv6(NftIpv4Hook::LocalIn, packet.bytes(), ingress_ifindex, 0)
                }
            };
            if !allowed || !self.confirm_accepted_packet(version) {
                return LocalInputVerdict::Drop;
            }
            // Without conntrack, Linux delivers each fragment through
            // PRE_ROUTING and LOCAL_IN before reassembling for raw/transport.
            // A forwarded fragment never reaches this local-delivery point.
            if version == IpVersion::Ipv6 && !self.ct_active_for(version) {
                match ipv6_fragment_offset(
                    packet.bytes(),
                    Ipv6DefragDomain::local_input(0, ingress_ifindex),
                ) {
                    Ok(None) => {}
                    Err(_) => return LocalInputVerdict::Drop,
                    Ok(Some(_)) => {
                        if self.work.try_reserve(1).is_err() {
                            return LocalInputVerdict::Drop;
                        }
                        let mut bytes = Vec::new();
                        if bytes.try_reserve_exact(packet.bytes().len()).is_err() {
                            return LocalInputVerdict::Drop;
                        }
                        bytes.extend_from_slice(packet.bytes());
                        self.work.push(RoutedIngressWork::Defrag {
                            netns: self.netns.clone(),
                            fragment: PendingIpFragment::Ipv6(PendingIpv6Fragment {
                                packet: bytes,
                                ingress_ifindex,
                                owner_ifindex: self.owner_ifindex,
                                source_hardware_addr,
                                source: DefragSource::LocalInput,
                                ct_context: None,
                                mark: self.mark.get(),
                            }),
                        });
                        return LocalInputVerdict::Drop;
                    }
                }
            }
        }

        let packet = packet.bytes();
        let (source, destination) = match version {
            IpVersion::Ipv4 => {
                let Ok(ipv4) = Ipv4Packet::new_checked(packet) else {
                    return LocalInputVerdict::Drop;
                };
                (
                    IpAddress::Ipv4(ipv4.src_addr()),
                    IpAddress::Ipv4(ipv4.dst_addr()),
                )
            }
            IpVersion::Ipv6 => {
                let Ok(ipv6) = Ipv6Packet::new_checked(packet) else {
                    return LocalInputVerdict::Drop;
                };
                (
                    IpAddress::Ipv6(ipv6.src_addr()),
                    IpAddress::Ipv6(ipv6.dst_addr()),
                )
            }
        };

        let mut recipients = Vec::new();
        let mut matched = false;
        for listener in self.raw_listeners {
            if listener.matches(version, protocol, source, destination, ingress_ifindex) {
                matched = true;
                if recipients.try_reserve(1).is_ok() {
                    recipients.push(listener.socket());
                }
            }
        }
        if !recipients.is_empty() && self.work.try_reserve(1).is_ok() {
            let mut bytes = Vec::new();
            if bytes.try_reserve_exact(packet.len()).is_ok() {
                bytes.extend_from_slice(packet);
                self.work.push(RoutedIngressWork::Raw(RawIngressWork {
                    sockets: recipients,
                    packet: bytes,
                    source,
                    destination,
                    protocol,
                    ingress_ifindex,
                    netns: self.netns.clone(),
                }));
            }
        }
        // Linux considers a raw protocol matched even when allocation or the
        // receive queue fails; an unknown protocol must not emit ICMP then.
        LocalInputVerdict::ExternalRaw { matched }
    }

    fn route_fragment(
        &mut self,
        packet: &mut RoutedIngressPacket<'_, '_>,
        meta: PacketMeta,
        source_hardware_addr: HardwareAddress,
    ) -> RouteInputVerdict {
        // smoltcp calls route_fragment before pre_routing for fragmented
        // IPv4. A pre-routed local handoff must neither repeat PRE_ROUTING
        // nor bypass the missing-CT-context check performed there for a
        // complete datagram.
        if self.stage.get() == IngressStage::PreRoutingDone {
            return if !self.pre_routed_context_valid(IpVersion::Ipv4) {
                RouteInputVerdict::Drop
            } else {
                RouteInputVerdict::Pass
            };
        }
        if self.ct_active_for(IpVersion::Ipv4) && !self.netns.ipv4_defrag_enabled() {
            // An active tracker cannot safely classify an isolated fragment.
            // Activation must prepare the defragmenter before this path runs.
            return RouteInputVerdict::Drop;
        }
        if self.ct_active_for(IpVersion::Ipv4)
            && self.netns.ipv4_defrag_enabled()
            && matches!(
                self.stage.get(),
                IngressStage::Pending | IngressStage::LocalOutput
            )
        {
            let ingress_ifindex = if meta.id != 0 {
                meta.id
            } else {
                self.owner_ifindex
            };
            if ingress_ifindex == 0 || self.work.try_reserve(1).is_err() {
                return RouteInputVerdict::Drop;
            }
            // Only offset zero owns the datagram's output CT identity. The
            // reassembler retains that origin even when later fragments
            // arrive first or complete the datagram on another poll round.
            let first_fragment = packet
                .bytes()
                .get(6..8)
                .is_some_and(|bytes| u16::from_be_bytes([bytes[0], bytes[1]]) & 0x1fff == 0);
            let ct_context = if self.ct_active_for(IpVersion::Ipv4)
                && self.stage.get() == IngressStage::LocalOutput
                && first_fragment
            {
                let Some(context) = self.packet_context.borrow_mut().take() else {
                    return RouteInputVerdict::Drop;
                };
                match Arc::try_new(context) {
                    Ok(context) => Some(context),
                    Err(_) => return RouteInputVerdict::Drop,
                }
            } else {
                None
            };
            let Ok(bytes) = packet.take_owned() else {
                return RouteInputVerdict::Drop;
            };
            self.work.push(RoutedIngressWork::Defrag {
                netns: self.netns.clone(),
                fragment: PendingIpFragment::Ipv4(PendingIpv4Fragment {
                    packet: bytes,
                    ingress_ifindex,
                    owner_ifindex: self.owner_ifindex,
                    source_hardware_addr,
                    source: if self.stage.get() == IngressStage::LocalOutput {
                        DefragSource::LocalOutput
                    } else {
                        DefragSource::LinkIngress
                    },
                    ct_context,
                    mark: self.mark.get(),
                    broadcast: first_fragment && self.handoff_broadcast.get(),
                }),
            });
            return RouteInputVerdict::Forward;
        }
        // Native transport payload and xt tcp inspect a first fragment only;
        // later fragments follow their respective BREAK/hotdrop semantics.
        // These expressions do not enable defragmentation by themselves.
        // Conntrack/NAT will require a separate, earlier defrag stage.
        let ingress_ifindex = if meta.id != 0 {
            meta.id
        } else {
            self.owner_ifindex
        };
        if !self.allows_ipv4(NftIpv4Hook::PreRouting, packet.bytes(), ingress_ifindex, 0) {
            return RouteInputVerdict::Drop;
        }
        if self.stage.get() == IngressStage::LocalOutput {
            return RouteInputVerdict::Pass;
        }
        let Some(routes) = self.routes else {
            return RouteInputVerdict::Pass;
        };
        let Ok(ipv4) = Ipv4Packet::new_checked(packet.bytes()) else {
            return RouteInputVerdict::Drop;
        };
        let destination = ipv4.dst_addr();
        if destination.is_multicast() {
            return RouteInputVerdict::Pass;
        }
        let ingress_ifindex = if meta.id != 0 {
            meta.id
        } else {
            self.owner_ifindex
        };
        let Some(route) = routes.lookup_ingress(destination.into(), ingress_ifindex) else {
            return RouteInputVerdict::Pass;
        };
        if route.matched.kind != RTN_UNICAST {
            return RouteInputVerdict::Pass;
        }
        self.route_input(packet, meta, source_hardware_addr)
    }
}
