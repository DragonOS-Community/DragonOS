//! One receive-side policy entry shared by direct and NAPI smoltcp polling.
//! Route handoff is added here only after the packet has passed IP validation.

use super::{
    conntrack::CtPacketContext,
    ipv6_defrag::{fragment_offset as ipv6_fragment_offset, DefragDomain as Ipv6DefragDomain},
    nftables::{NftDeviceNames, NftIpv4Hook, RulesetSnapshot},
    route::{OutputRouteGuard, RTN_BROADCAST, RTN_LOCAL, RTN_UNICAST},
    socket::inet::raw::{RawIngressBatch, RawIngressWork},
};
use crate::driver::net::{
    inject_owned_local_ip_packet_if_epoch, types::InterfaceFlags, veth::VethInterface, Iface,
    IfaceCommon, LocalPacketOrigin, PacketBuffer,
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
        IpVersion, Ipv4Address, Ipv4Packet, Ipv6ExtHeader, Ipv6Packet,
    },
};
use system_error::SystemError;

pub(crate) mod bridge;
mod hooks;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum IngressStage {
    #[default]
    Pending,
    PreRoutingDone,
    BridgePreRoutingDone,
    LocalOutput,
    LocalInputDone,
}

/// The stage cell is private to one synchronous smoltcp poll. A local RX
/// token changes it only while its bytes are being consumed; a physical RX
/// token leaves it at Pending. No packet-level state is stored globally.
pub(crate) struct NetIngressFilter<'a> {
    ruleset: &'a Arc<RulesetSnapshot>,
    netns: &'a Arc<NetNamespace>,
    owner_ifindex: u32,
    owner_epoch: u64,
    owner: &'a IfaceCommon,
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
    raw_batch: Option<&'a mut RawIngressBatch>,
    raw_retirement: &'a mut Vec<RawIngressWork>,
    icmp_error_items: usize,
    icmp_error_bytes: usize,
}

/// Named poll inputs keep the two direct/NAPI call sites in sync as receive
/// policy grows, without adding state to an interface or a network namespace.
pub(crate) struct NetIngressFilterInit<'a> {
    pub(crate) ruleset: &'a Arc<RulesetSnapshot>,
    pub(crate) netns: &'a Arc<NetNamespace>,
    pub(crate) owner_ifindex: u32,
    pub(crate) owner_epoch: u64,
    pub(crate) owner: &'a IfaceCommon,
    pub(crate) device_names: &'a NftDeviceNames,
    pub(crate) stage: &'a Cell<IngressStage>,
    pub(crate) handoff_broadcast: &'a Cell<bool>,
    pub(crate) mark: &'a Cell<u32>,
    pub(crate) forward_fragment: &'a Cell<Option<super::forward_mtu::ReassembledForwardInfo>>,
    pub(crate) packet_context: &'a RefCell<Option<CtPacketContext>>,
    pub(crate) routes: Option<&'a OutputRouteGuard<'a>>,
    pub(crate) fib_routes: Option<&'a OutputRouteGuard<'a>>,
    pub(crate) work: &'a mut Vec<RoutedIngressWork>,
    pub(crate) raw_batch: Option<&'a mut RawIngressBatch>,
    pub(crate) raw_retirement: &'a mut Vec<RawIngressWork>,
}

/// A route decision and owned datagram are transferred out of the receive
/// callback together. No target interface is entered while the source's
/// smoltcp, socket-set, or FIB locks are held.
pub(crate) enum RoutedIngressWork {
    RouterAdvertisement {
        netns: Arc<NetNamespace>,
        advertisement: super::route::ra::Advertisement,
    },
    IcmpError {
        netns: Arc<NetNamespace>,
        feedback: super::icmp_error::IcmpErrorFeedback,
    },
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
        packet: PacketBuffer,
        ct_context: CtPacketContext,
        mark: u32,
    },
    Forward {
        target: Arc<dyn Iface>,
        target_epoch: u64,
        next_hop: IpAddress,
        packet: PacketBuffer,
        fragment_info: Option<super::forward_mtu::ReassembledForwardInfo>,
        netns: Arc<NetNamespace>,
        ct_context: CtPacketContext,
    },
    MtuErrorIpv4 {
        netns: Arc<NetNamespace>,
        packet: PacketBuffer,
        mtu: usize,
        ct_context: Option<CtPacketContext>,
    },
    MtuErrorIpv6 {
        netns: Arc<NetNamespace>,
        packet: PacketBuffer,
        mtu: usize,
        ct_context: Option<CtPacketContext>,
    },
}

impl RoutedIngressWork {
    pub(crate) fn execute(self) {
        let result = match self {
            Self::RouterAdvertisement {
                netns,
                advertisement,
            } => {
                netns.queue_router_advertisement(advertisement);
                return;
            }
            Self::IcmpError { netns, feedback } => {
                super::icmp_error::execute(&netns, feedback);
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
            owner_ifindex,
            owner_epoch,
            owner,
            device_names,
            stage,
            handoff_broadcast,
            mark,
            forward_fragment,
            packet_context,
            routes,
            fib_routes,
            work,
            raw_batch,
            raw_retirement,
        } = init;
        Self {
            ruleset,
            netns,
            owner_ifindex,
            owner_epoch,
            owner,
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
            raw_batch,
            raw_retirement,
            icmp_error_items: 0,
            icmp_error_bytes: 0,
        }
    }

    fn hooks(&self) -> hooks::IngressHooks<'_> {
        hooks::IngressHooks {
            ruleset: self.ruleset,
            netns: self.netns,
            device_names: hooks::HookDeviceNames::Namespace(self.device_names),
            stage: self.stage.get(),
            mark: self.mark,
            packet_context: self.packet_context,
            fib_routes: self.fib_routes,
        }
    }

    fn pre_routed_context_valid(&self, version: IpVersion) -> bool {
        !self.hooks().ct_active_for(version) || self.packet_context.borrow().is_some()
    }

    fn take_handoff_context(&self, version: IpVersion) -> Option<CtPacketContext> {
        if self.hooks().ct_active_for(version) {
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
        let Some(route) = routes.lookup_ingress(destination.into(), ingress_ifindex, Some(source))
        else {
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
                let Ok(packet) = packet.take_owned().map_err(|_| ()).and_then(|bytes| {
                    crate::driver::net::packet_storage::take(bytes).map_err(|_| ())
                }) else {
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
                    let Ok(packet) = packet.take_owned().map_err(|_| ()).and_then(|bytes| {
                        crate::driver::net::packet_storage::take(bytes).map_err(|_| ())
                    }) else {
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
                let Ok(mut packet) = packet.take_owned().map_err(|_| ()).and_then(|bytes| {
                    crate::driver::net::packet_storage::take(bytes).map_err(|_| ())
                }) else {
                    return RouteInputVerdict::Drop;
                };
                let Ok(mut forwarded) = Ipv6Packet::new_checked(packet.as_mut_slice()) else {
                    return RouteInputVerdict::Drop;
                };
                forwarded.set_hop_limit(forwarded.hop_limit() - 1);
                let allowed = self.hooks().allows_ipv6(
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
                    self.hooks().allows_ipv6_nat(
                        NftIpv4Hook::PostRouting,
                        packet.as_mut_slice(),
                        ingress_ifindex,
                        route.oif,
                        Some(route.next_hop),
                    )
                } else {
                    self.hooks().allows_ipv6(
                        NftIpv4Hook::PostRouting,
                        packet.as_slice(),
                        ingress_ifindex,
                        route.oif,
                    )
                };
                if !post_allowed {
                    return RouteInputVerdict::Drop;
                }
                let Some(ct_context) = self
                    .hooks()
                    .confirm_accepted_packet_context(IpVersion::Ipv6)
                else {
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
    fn ipv6_ndisc_router(&self) -> bool {
        self.owner.ipv6_forwarding() != 0
    }

    fn arp_reply_allowed(&self, source: Ipv4Address, target: Ipv4Address) -> Option<bool> {
        // Linux's default Ethernet ARP policy does not expose loopback or
        // multicast targets. IP AnyIP admission is not proxy-ARP permission.
        let sender_allowed =
            source.is_unspecified() || (!source.is_loopback() && source.octets()[0] != 0);
        let allowed = sender_allowed
            && !target.is_unspecified()
            && !target.is_loopback()
            && !target.is_multicast()
            && target != Ipv4Address::BROADCAST
            && self.fib_routes.is_some_and(|routes| {
                routes
                    .lookup_ingress(target.into(), self.owner_ifindex, None)
                    .is_some_and(|route| route.matched.kind == RTN_LOCAL)
            });
        // Never fall back to smoltcp's AnyIP ARP replies when this integration
        // cannot establish ownership from its pre-acquired namespace view.
        Some(allowed)
    }

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

    fn applies_to(&self, _version: IpVersion) -> bool {
        // DragonOS owns raw RX delivery, including socket filters, address
        // matching and receive-memory accounting. An empty listener snapshot
        // must not enable smoltcp's independent, unfiltered raw queue: a raw
        // socket can be attached after the snapshot but before polling takes
        // the socket-set lock. Keep one ingress owner even during publication.
        // Empty rules and listener lists already avoid packet copies below.
        true
    }

    fn pre_routing_ipv6_defrag(
        &mut self,
        packet: &mut IngressPacket<'_>,
        meta: PacketMeta,
        source_hardware_addr: HardwareAddress,
    ) -> PreRoutingVerdict {
        if matches!(
            self.stage.get(),
            IngressStage::LocalInputDone | IngressStage::BridgePreRoutingDone
        ) || !self.hooks().ct_active_for(IpVersion::Ipv6)
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
        let ct_context = if self.hooks().ct_active_for(IpVersion::Ipv6)
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
        let Ok(bytes) = packet
            .take_owned()
            .map_err(|_| ())
            .and_then(|bytes| crate::driver::net::packet_storage::take(bytes).map_err(|_| ()))
        else {
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
            IngressStage::PreRoutingDone
                | IngressStage::LocalInputDone
                | IngressStage::BridgePreRoutingDone
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
                        self.hooks().allows_ipv4_nat(
                            NftIpv4Hook::PreRouting,
                            bytes,
                            ingress_ifindex,
                            0,
                            None,
                        )
                    })
                } else {
                    self.hooks().allows_ipv4(
                        NftIpv4Hook::PreRouting,
                        packet.bytes(),
                        ingress_ifindex,
                        0,
                    )
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
                        self.hooks().allows_ipv6_nat(
                            NftIpv4Hook::PreRouting,
                            bytes,
                            ingress_ifindex,
                            0,
                            None,
                        )
                    })
                } else {
                    self.hooks().allows_ipv6(
                        NftIpv4Hook::PreRouting,
                        packet.bytes(),
                        ingress_ifindex,
                        0,
                    )
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
        let ingress_ifindex = if meta.id != 0 {
            meta.id
        } else {
            self.owner_ifindex
        };
        let Ok(ipv4) = Ipv4Packet::new_checked(packet.bytes()) else {
            return RouteInputVerdict::Drop;
        };
        let source = ipv4.src_addr();
        let destination = ipv4.dst_addr();
        let routes = if ingress_ifindex != crate::net::LOOPBACK_IFINDEX as u32
            && (source.is_loopback() || destination.is_loopback())
        {
            let Some(routes) = self.fib_routes else {
                return RouteInputVerdict::Drop;
            };
            if !routes.allows_ipv4_loopback_ingress(ingress_ifindex, source) {
                return RouteInputVerdict::Drop;
            }
            routes
        } else {
            let Some(routes) = self.routes else {
                return RouteInputVerdict::Pass;
            };
            routes
        };
        let source = source.octets();
        if source[0] == 0 && (source != [0; 4] || destination.octets() != [255; 4]) {
            return RouteInputVerdict::Drop;
        }
        // An explicitly configured broadcast address is not part of
        // smoltcp's CIDR-derived source classification. The local FIB is the
        // authority for that address, so it cannot be accepted as a unicast
        // source merely because its octets have an ordinary host shape.
        if routes
            .lookup_ingress(IpAddress::Ipv4(ipv4.src_addr()), ingress_ifindex, None)
            .is_some_and(|route| route.matched.kind == RTN_BROADCAST)
        {
            return RouteInputVerdict::Drop;
        }
        // Multicast routing is not configured by this FIB. A default unicast
        // route must never steal IGMP or joined-group UDP from the local stack.
        if destination.is_multicast() {
            return RouteInputVerdict::Pass;
        }
        let Some(route) = routes.lookup_ingress(destination.into(), ingress_ifindex, None) else {
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
                let Ok(packet) = packet.take_owned().map_err(|_| ()).and_then(|bytes| {
                    crate::driver::net::packet_storage::take(bytes).map_err(|_| ())
                }) else {
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
                    let Ok(packet) = packet.take_owned().map_err(|_| ()).and_then(|bytes| {
                        crate::driver::net::packet_storage::take(bytes).map_err(|_| ())
                    }) else {
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
                let Ok(mut packet) = packet.take_owned().map_err(|_| ()).and_then(|bytes| {
                    crate::driver::net::packet_storage::take(bytes).map_err(|_| ())
                }) else {
                    return RouteInputVerdict::Drop;
                };
                let Ok(mut forwarded) = Ipv4Packet::new_checked(packet.as_mut_slice()) else {
                    return RouteInputVerdict::Drop;
                };
                forwarded.set_hop_limit(forwarded.hop_limit() - 1);
                forwarded.fill_checksum();
                let allowed = self.hooks().allows_ipv4(
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
                    self.hooks().allows_ipv4_nat(
                        NftIpv4Hook::PostRouting,
                        packet.as_mut_slice(),
                        ingress_ifindex,
                        route.oif,
                        Some(route.next_hop),
                    )
                } else {
                    self.hooks().allows_ipv4(
                        NftIpv4Hook::PostRouting,
                        packet.as_slice(),
                        ingress_ifindex,
                        route.oif,
                    )
                };
                if !post_allowed {
                    return RouteInputVerdict::Drop;
                }
                let Some(ct_context) = self
                    .hooks()
                    .confirm_accepted_packet_context(IpVersion::Ipv4)
                else {
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
        transport_offset: usize,
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
                        self.hooks().allows_ipv4_nat(
                            NftIpv4Hook::LocalIn,
                            bytes,
                            ingress_ifindex,
                            0,
                            None,
                        )
                    })
                }
                IpVersion::Ipv6 if self.ruleset.hook_may_nat(version, NftIpv4Hook::LocalIn) => {
                    packet.writable().is_ok_and(|bytes| {
                        self.hooks().allows_ipv6_nat(
                            NftIpv4Hook::LocalIn,
                            bytes,
                            ingress_ifindex,
                            0,
                            None,
                        )
                    })
                }
                IpVersion::Ipv4 => self.hooks().allows_ipv4(
                    NftIpv4Hook::LocalIn,
                    packet.bytes(),
                    ingress_ifindex,
                    0,
                ),
                IpVersion::Ipv6 => self.hooks().allows_ipv6(
                    NftIpv4Hook::LocalIn,
                    packet.bytes(),
                    ingress_ifindex,
                    0,
                ),
            };
            if !allowed || !self.hooks().confirm_accepted_packet(version) {
                return LocalInputVerdict::Drop;
            }
            // Without conntrack, Linux delivers each fragment through
            // PRE_ROUTING and LOCAL_IN before reassembling for raw/transport.
            // A forwarded fragment never reaches this local-delivery point.
            if version == IpVersion::Ipv6 && !self.hooks().ct_active_for(version) {
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
                        let Ok(bytes) = PacketBuffer::try_from_slice(packet.bytes()) else {
                            return LocalInputVerdict::Drop;
                        };
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
        // Reassembly removes the IPv6 Fragment Header. A Neighbor
        // Solicitation used for DAD must still arrive unfragmented.
        if version == IpVersion::Ipv6
            && protocol == IpProtocol::Icmpv6
            && (self.forward_fragment.get().is_some()
                || matches!(
                    self.stage.get(),
                    IngressStage::LocalInputDone | IngressStage::LocalOutput
                ))
            && Ipv6Packet::new_checked(packet)
                .is_ok_and(|header| header.src_addr().is_unspecified())
        {
            return LocalInputVerdict::Drop;
        }
        // RA is link control, so use the same device that delivered the
        // packet locally (the bridge master for bridged advertisements).
        if protocol == IpProtocol::Icmpv6
            && ingress_ifindex == self.owner_ifindex
            && !matches!(
                self.stage.get(),
                IngressStage::LocalInputDone | IngressStage::LocalOutput
            )
            && self.forward_fragment.get().is_none()
        {
            super::address::ipv6_autoconf::receive(
                self.owner,
                self.netns,
                packet,
                transport_offset,
                ingress_ifindex,
            );
            if let Some(advertisement) = super::route::ra::parse(
                packet,
                transport_offset,
                ingress_ifindex,
                self.owner_epoch,
                self.owner,
            ) {
                if self.work.try_reserve(1).is_ok() {
                    self.work.push(RoutedIngressWork::RouterAdvertisement {
                        netns: self.netns.clone(),
                        advertisement,
                    });
                }
            }
        }
        // Control admission precedes allocation. Ordinary raw delivery below
        // remains independent when the ICMP control budget is exhausted.
        if matches!(protocol, IpProtocol::Icmp | IpProtocol::Icmpv6)
            && self.icmp_error_items < 64
            && packet.len() <= (64 * 1024usize).saturating_sub(self.icmp_error_bytes)
            && self.work.try_reserve(1).is_ok()
        {
            if let Some(feedback) = super::icmp_error::parse(packet, ingress_ifindex) {
                self.icmp_error_items += 1;
                self.icmp_error_bytes += feedback.quote.capacity();
                if feedback.protocol == IpProtocol::Tcp {
                    // TCP segments enter their protocol queue in this same
                    // device poll. Publish control input now as well, or a
                    // later segment could overtake deferred ICMP work. Only
                    // enqueue here; all socket callbacks run in TCP task context.
                    self.netns.tcp_stack().queue_icmp_feedback(feedback);
                } else {
                    self.work.push(RoutedIngressWork::IcmpError {
                        netns: self.netns.clone(),
                        feedback,
                    });
                }
            }
        }
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

        // Allocate retirement work before upgrading any socket reference.
        // A poll-start listener snapshot can miss sockets registered before
        // this actual packet arrived, even when the packet was sent by one.
        if self.raw_retirement.try_reserve(1).is_err() {
            return LocalInputVerdict::Drop;
        }
        let Ok(mut sockets) = super::socket::inet::raw::snapshot_raw_ingress_sockets(self.netns)
        else {
            return LocalInputVerdict::Drop;
        };
        // Partition in place: only admission-time matches receive bytes, but
        // unmatched strong pins also survive until lock-free retirement.
        let mut matched = 0;
        for index in 0..sockets.len() {
            if sockets[index].matches_ingress(
                version,
                protocol,
                source,
                destination,
                ingress_ifindex,
            ) {
                sockets.swap(index, matched);
                matched += 1;
            }
        }
        if !sockets.is_empty() {
            let work = RawIngressWork {
                sockets,
                matched,
                packet: PacketBuffer::new(),
                source,
                destination,
                protocol,
                ingress_ifindex,
                _netns: self.netns.clone(),
            };
            // Nonmatching candidates and all resource-pressure paths retain
            // their pins locally until the interface and poll guards are gone.
            let retirement = if matched != 0 {
                match self.raw_batch.as_mut() {
                    Some(batch) => batch.record(work, packet, &self.netns.raw_ingress).err(),
                    None => Some(work),
                }
            } else {
                Some(work)
            };
            if let Some(work) = retirement {
                self.raw_retirement.push(work);
            }
        }
        // Linux considers a raw protocol matched even when allocation or the
        // receive queue fails; an unknown protocol must not emit ICMP then.
        LocalInputVerdict::ExternalRaw {
            matched: matched != 0,
        }
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
        if self.stage.get() == IngressStage::BridgePreRoutingDone {
            return if self.pre_routed_context_valid(IpVersion::Ipv4) {
                self.route_input(packet, meta, source_hardware_addr)
            } else {
                RouteInputVerdict::Drop
            };
        }
        if self.hooks().ct_active_for(IpVersion::Ipv4) && !self.netns.ipv4_defrag_enabled() {
            // An active tracker cannot safely classify an isolated fragment.
            // Activation must prepare the defragmenter before this path runs.
            return RouteInputVerdict::Drop;
        }
        if self.hooks().ct_active_for(IpVersion::Ipv4)
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
            let ct_context = if self.hooks().ct_active_for(IpVersion::Ipv4)
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
            let Ok(bytes) = packet
                .take_owned()
                .map_err(|_| ())
                .and_then(|bytes| crate::driver::net::packet_storage::take(bytes).map_err(|_| ()))
            else {
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
        if !self
            .hooks()
            .allows_ipv4(NftIpv4Hook::PreRouting, packet.bytes(), ingress_ifindex, 0)
        {
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
        let Some(route) = routes.lookup_ingress(destination.into(), ingress_ifindex, None) else {
            return RouteInputVerdict::Pass;
        };
        if route.matched.kind != RTN_UNICAST {
            return RouteInputVerdict::Pass;
        }
        self.route_input(packet, meta, source_hardware_addr)
    }
}
