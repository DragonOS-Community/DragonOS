//! Bridge IP ingress and logical interfaces for the Netfilter hook sequence.

use super::{BridgeDriver, BridgeEgress, BridgeIface, BridgePort, BridgePortId};
use crate::process::namespace::net_namespace::{
    BridgeFragmentIngress, DefragSource, NetNamespace, PendingIpFragment, PendingIpv4Fragment,
    PendingIpv6Fragment,
};
use crate::{
    driver::net::{
        iface::{enqueue_ip_input, PreparedFrameTxToken},
        local_queue::{
            LocalInputPacket, LocalOutputDisposition, LocalOutputPacket, OutputCtContext,
        },
        Iface, PacketBuffer, RouteSendError,
    },
    net::{
        conntrack::CtPacketContext,
        ingress::{
            bridge::{BridgeIpHooks, BridgePacketContext, IpFrame},
            IngressStage,
        },
        socket::packet::{classify_packet, deliver_to_packet_sockets, PacketType},
    },
};
use alloc::{boxed::Box, sync::Arc};
use core::{cell::Cell, sync::atomic::Ordering};
use smoltcp::wire::{
    EthernetAddress, EthernetFrame, EthernetProtocol, IpAddress, IpVersion, Ipv4Packet,
};
use system_error::SystemError;

#[derive(Debug, Clone, Copy)]
struct BridgeIngressPort {
    id: BridgePortId,
    generation: u64,
}

/// The unresolved MAC is the only asynchronous step after PRE_ROUTING.
#[derive(Debug)]
pub(in crate::driver::net) struct BridgeDnat {
    ingress_port: BridgeIngressPort,
    generation: usize,
    context: BridgePacketContext,
    fragment: Option<crate::net::forward_mtu::ReassembledForwardInfo>,
    version: IpVersion,
}

impl BridgeDriver {
    /// Returns true when the IP policy path owns the ingress packet.
    pub(super) fn handle_ip_frame(
        &self,
        ingress_port: BridgePortId,
        frame: &[u8],
        fragment: Option<crate::net::forward_mtu::ReassembledForwardInfo>,
        expected_port_generation: Option<u64>,
    ) -> bool {
        let Ok(header) = EthernetFrame::new_checked(frame) else {
            return false;
        };
        let version = match header.ethertype() {
            EthernetProtocol::Ipv4 => IpVersion::Ipv4,
            EthernetProtocol::Ipv6 => IpVersion::Ipv6,
            _ => return false,
        };
        let Some(netns) = self.netns() else {
            return false;
        };
        if fragment.is_none() && !netns.bridge_netfilter_enabled(version) {
            return false;
        }
        let Some(bridge) = self.local_iface() else {
            return false;
        };
        let hooks = BridgeIpHooks::new(&netns, bridge.as_ref(), version);
        let ingress = self.inner.lock().ports.get(&ingress_port).cloned();
        let Some(_ingress_activity) = ingress.as_ref().and_then(BridgePort::enter) else {
            return true;
        };
        let port_generation = ingress
            .as_ref()
            .unwrap()
            .bridge_enable
            .bridge_ingress_generation();
        if expected_port_generation.is_some_and(|expected| expected != port_generation) {
            return true;
        }
        let Some(mut frame) = IpFrame::checked(frame, version) else {
            return true;
        };
        if !hooks.has_hooks() && fragment.is_none() {
            let Ok(decision) = self.inner.lock().select_egress(ingress_port, frame.bytes()) else {
                return true;
            };
            if decision.local {
                self.enqueue_local(frame.bytes(), decision.local_stack_dst);
            }
            self.transmit_egress(decision.egress, frame.bytes());
            return true;
        }
        if fragment.is_none()
            && hooks.needs_conntrack()
            && self.queue_bridge_fragment(
                &netns,
                bridge.nic_id() as u32,
                ingress_port,
                port_generation,
                &frame,
                version,
            )
        {
            return true;
        }
        let original_destination = frame.destination(version);
        let context = BridgePacketContext::default();
        if !hooks.pre_routing(&mut frame, &context) {
            return true;
        }
        if context.conntrack.borrow().is_none() {
            context.conntrack.replace(Some(CtPacketContext::Untracked));
        }
        if frame.destination(version) != original_destination {
            self.handle_bridge_dnat(
                &bridge,
                &hooks,
                BridgeIngressPort {
                    id: ingress_port,
                    generation: port_generation,
                },
                frame,
                context,
                fragment,
            );
        } else {
            self.finish_bridge_ip(&bridge, &hooks, ingress_port, &frame, context, fragment);
        }
        true
    }

    fn queue_bridge_fragment(
        &self,
        netns: &NetNamespace,
        bridge_ifindex: u32,
        ingress_port: BridgePortId,
        port_generation: u64,
        frame: &IpFrame<'_>,
        version: IpVersion,
    ) -> bool {
        let generation = self.ingress_generation.load(Ordering::Acquire);
        let fragmented = match version {
            IpVersion::Ipv4 => {
                let packet = Ipv4Packet::new_unchecked(frame.ip());
                packet.more_frags() || packet.frag_offset() != 0
            }
            IpVersion::Ipv6 => match crate::net::ipv6_defrag::fragment_offset(
                frame.ip(),
                crate::net::ipv6_defrag::DefragDomain::bridge_ingress(bridge_ifindex, generation),
            ) {
                Ok(offset) => offset.is_some(),
                Err(_) => return true,
            },
        };
        if !fragmented {
            return false;
        }
        let Ok(packet) = PacketBuffer::try_from_slice(frame.ip()) else {
            return true;
        };
        let ethernet = EthernetFrame::new_unchecked(frame.bytes());
        let source = DefragSource::BridgeIngress(BridgeFragmentIngress {
            bridge_ifindex,
            generation,
            port: ingress_port,
            port_generation,
            ethernet_header: frame.bytes()[..14].try_into().unwrap(),
        });
        let source_hardware_addr = smoltcp::wire::HardwareAddress::Ethernet(ethernet.src_addr());
        let fragment = match version {
            IpVersion::Ipv4 => PendingIpFragment::Ipv4(PendingIpv4Fragment {
                packet,
                ingress_ifindex: bridge_ifindex,
                owner_ifindex: bridge_ifindex,
                source_hardware_addr,
                source,
                ct_context: None,
                mark: 0,
                broadcast: false,
            }),
            IpVersion::Ipv6 => PendingIpFragment::Ipv6(PendingIpv6Fragment {
                packet,
                ingress_ifindex: bridge_ifindex,
                owner_ifindex: bridge_ifindex,
                source_hardware_addr,
                source,
                ct_context: None,
                mark: 0,
            }),
        };
        let _ = netns.queue_ip_fragment(fragment);
        true
    }

    pub(crate) fn resume_reassembled_ip(
        &self,
        first: BridgeFragmentIngress,
        completion: BridgeFragmentIngress,
        packet: PacketBuffer,
        max_original_fragment_len: usize,
    ) {
        let Some(_activity) = self.enter_data_path() else {
            return;
        };
        if self.ingress_generation.load(Ordering::Acquire) != completion.generation {
            return;
        }
        let Ok(mut frame) = PacketBuffer::try_with_capacity(14 + packet.len()) else {
            return;
        };
        if frame.extend_from_slice(&first.ethernet_header).is_err()
            || frame.extend_from_slice(&packet).is_err()
        {
            return;
        }
        self.handle_ip_frame(
            first.port,
            &frame,
            Some(crate::net::forward_mtu::ReassembledForwardInfo {
                max_original_fragment_len,
            }),
            Some(first.port_generation),
        );
    }

    fn handle_bridge_dnat(
        &self,
        bridge: &Arc<BridgeIface>,
        hooks: &BridgeIpHooks<'_>,
        ingress_port: BridgeIngressPort,
        mut frame: IpFrame<'_>,
        context: BridgePacketContext,
        fragment: Option<crate::net::forward_mtu::ReassembledForwardInfo>,
    ) {
        use crate::net::route::{lookup_ingress, RTN_BROADCAST, RTN_LOCAL, RTN_UNICAST};

        let version = hooks.version();

        let Some(netns) = bridge.net_namespace() else {
            return;
        };
        let destination = frame.destination(version);
        let ingress_ifindex = bridge.nic_id() as u32;
        let route = if let IpAddress::Ipv4(destination) = destination {
            let source = Ipv4Packet::new_unchecked(frame.ip()).src_addr();
            if source.is_loopback() || destination.is_loopback() {
                let router = netns.router();
                let routes = crate::net::route::lock_output_routes(&router, netns.device_list());
                if !routes.allows_ipv4_loopback_ingress(ingress_ifindex, source) {
                    return;
                }
                routes.lookup_ingress(destination.into(), ingress_ifindex)
            } else {
                lookup_ingress(&netns, destination.into(), ingress_ifindex)
            }
        } else {
            lookup_ingress(&netns, destination, ingress_ifindex)
        };
        let Some(route) = route else {
            return;
        };
        if matches!(route.matched.kind, RTN_LOCAL | RTN_BROADCAST)
            || (route.matched.kind == RTN_UNICAST && route.oif != bridge.nic_id() as u32)
        {
            if frame.set_destination_mac(bridge.mac()).is_err() {
                return;
            }
            let source = EthernetFrame::new_unchecked(frame.bytes()).src_addr();
            if !self.inner.lock().learn_source(ingress_port.id, source) {
                return;
            }
            self.enqueue_bridge_ip(bridge, &frame, None, context, fragment);
        } else if route.matched.kind == RTN_UNICAST {
            match probe_next_hop(bridge.as_ref(), &netns, route.next_hop, frame.ip()) {
                Ok(mac) => {
                    if frame.set_destination_mac(mac).is_ok() {
                        self.finish_bridge_ip(
                            bridge,
                            hooks,
                            ingress_port.id,
                            &frame,
                            context,
                            fragment,
                        );
                    }
                }
                Err(RouteSendError::RetryAt {
                    retry_at,
                    probe_sent,
                }) => {
                    let pending = BridgeDnat {
                        ingress_port,
                        generation: self.ingress_generation.load(Ordering::Acquire),
                        context,
                        fragment,
                        version,
                    };
                    self.enqueue_bridge_dnat(
                        bridge.as_ref(),
                        route.next_hop,
                        frame.bytes(),
                        pending,
                        retry_at,
                        probe_sent,
                    );
                }
                Err(RouteSendError::Failed(_)) => {}
            }
        }
    }

    fn enqueue_bridge_dnat(
        &self,
        bridge: &BridgeIface,
        next_hop: IpAddress,
        frame: &[u8],
        pending: BridgeDnat,
        retry_at: smoltcp::time::Instant,
        probe_sent: bool,
    ) {
        let queue = &bridge.common().local_input_queue;
        let Some(mut reservation) = queue.reserve_output() else {
            return;
        };
        let Ok(frame) = PacketBuffer::try_from_slice(frame) else {
            return;
        };
        if !reservation.try_resize(frame.capacity()) {
            return;
        }
        let Ok(pending) = Box::try_new(pending) else {
            return;
        };
        let packet = LocalOutputPacket {
            medium: smoltcp::phy::Medium::Ethernet,
            meta: smoltcp::phy::PacketMeta::default(),
            disposition: LocalOutputDisposition::Routed {
                oif: bridge.nic_id() as u32,
                next_hop,
                ip_mtu: bridge.mtu(),
            },
            frame,
            ct_context: OutputCtContext::Untracked,
            mark: 0,
            prepared_ip: None,
            forward_mtu_feedback: None,
            bridge_dnat: Some(pending),
            _charge: None,
        };
        if reservation
            .commit_deferred_packet(packet, retry_at, probe_sent, false)
            .is_ok()
        {
            let retry_at = if bridge
                .common()
                .release_resolved_neighbor_after_enqueue(bridge.nic_id() as u32, next_hop)
            {
                crate::time::Instant::now().into()
            } else {
                retry_at
            };
            bridge.common().schedule_registered_local_output(retry_at);
        }
    }

    pub(in crate::driver::net) fn resume_bridge_dnat(
        &self,
        bridge: &BridgeIface,
        next_hop: IpAddress,
        original: &[u8],
        pending: &BridgeDnat,
    ) -> Result<(), RouteSendError> {
        let _activity = self.enter_data_path().ok_or(SystemError::ENETDOWN)?;
        if self.ingress_generation.load(Ordering::Acquire) != pending.generation {
            return Err(SystemError::ENODEV.into());
        }
        let ingress = self
            .inner
            .lock()
            .ports
            .get(&pending.ingress_port.id)
            .cloned();
        let _ingress_activity = ingress
            .as_ref()
            .and_then(BridgePort::enter)
            .ok_or(SystemError::ENODEV)?;
        if ingress
            .as_ref()
            .unwrap()
            .bridge_enable
            .bridge_ingress_generation()
            != pending.ingress_port.generation
        {
            return Err(SystemError::ENODEV.into());
        }
        let netns = bridge.net_namespace().ok_or(SystemError::ENODEV)?;
        let mut frame = IpFrame::checked(original, pending.version).ok_or(SystemError::EINVAL)?;
        let route = crate::net::route::lookup_ingress(
            &netns,
            frame.destination(pending.version),
            bridge.nic_id() as u32,
        )
        .ok_or(SystemError::ENETUNREACH)?;
        if route.oif != bridge.nic_id() as u32
            || route.next_hop != next_hop
            || route.matched.kind != crate::net::route::RTN_UNICAST
        {
            return Err(SystemError::ENETUNREACH.into());
        }
        let mac = probe_next_hop(bridge, &netns, next_hop, frame.ip())?;
        frame.set_destination_mac(mac)?;
        let hooks = BridgeIpHooks::new(&netns, bridge, pending.version);
        let local = self.local_iface().ok_or(SystemError::ENODEV)?;
        self.finish_bridge_ip(
            &local,
            &hooks,
            pending.ingress_port.id,
            &frame,
            pending.context.fork(),
            pending.fragment,
        );
        Ok(())
    }

    fn finish_bridge_ip(
        &self,
        bridge: &Arc<BridgeIface>,
        hooks: &BridgeIpHooks<'_>,
        ingress_port: BridgePortId,
        frame: &IpFrame<'_>,
        context: BridgePacketContext,
        fragment: Option<crate::net::forward_mtu::ReassembledForwardInfo>,
    ) {
        let Ok(decision) = self.inner.lock().select_egress(ingress_port, frame.bytes()) else {
            return;
        };
        let send = |port: &BridgePort| {
            if !port.can_forward() {
                return;
            }
            let mut copy = frame.fork();
            let packet_context = context.fork();
            if !hooks.forward(&mut copy, &packet_context) {
                return;
            }
            let Some(confirmed) = hooks.confirm(&packet_context) else {
                return;
            };
            context.conntrack.replace(Some(confirmed));
            transmit_ip_frames(port, bridge, &copy, fragment);
        };
        match decision.egress {
            BridgeEgress::None => {}
            BridgeEgress::One(port) => send(&port),
            BridgeEgress::Flood(ports) => {
                for port in &ports {
                    send(port);
                }
            }
        }
        if decision.local {
            self.enqueue_bridge_ip(bridge, frame, decision.local_stack_dst, context, fragment);
        }
    }

    fn enqueue_bridge_ip(
        &self,
        bridge: &Arc<BridgeIface>,
        frame: &IpFrame<'_>,
        local_stack_dst: Option<EthernetAddress>,
        context: BridgePacketContext,
        fragment: Option<crate::net::forward_mtu::ReassembledForwardInfo>,
    ) {
        let iface: Arc<dyn Iface> = bridge.clone();
        let ethernet = EthernetFrame::new_unchecked(frame.bytes());
        let packet_type = if local_stack_dst.is_some() {
            PacketType::Host
        } else {
            classify_packet(frame.bytes(), &iface)
        };
        deliver_to_packet_sockets(&iface, frame.bytes(), packet_type);
        let Ok(ip_packet) = PacketBuffer::try_from_slice(frame.ip()) else {
            return;
        };
        let packet = LocalInputPacket {
            ingress_ifindex: bridge.nic_id() as u32,
            ingress_stage: IngressStage::BridgePreRoutingDone,
            broadcast: false,
            destination_mac: local_stack_dst.unwrap_or_else(|| ethernet.dst_addr()),
            source_mac: ethernet.src_addr(),
            ip_packet,
            ct_context: Some(
                context
                    .conntrack
                    .into_inner()
                    .unwrap_or(CtPacketContext::Untracked),
            ),
            mark: context.mark.get(),
            forward_fragment: fragment,
        };
        let _ = enqueue_ip_input(bridge.as_ref(), packet, None);
    }
}

fn probe_next_hop(
    bridge: &BridgeIface,
    netns: &Arc<NetNamespace>,
    next_hop: IpAddress,
    ip_packet: &[u8],
) -> Result<EthernetAddress, RouteSendError> {
    use smoltcp::{iface::IpPacketDispatchError, wire::HardwareAddress};

    let permanent = crate::net::neighbor::lookup(netns, bridge.nic_id() as u32, next_hop)
        .map(HardwareAddress::Ethernet);
    let arp_source = (next_hop.version() == IpVersion::Ipv4)
        .then(|| Ipv4Packet::new_unchecked(ip_packet).src_addr())
        .filter(|address| {
            crate::net::route::ipv4_addr_type(netns, *address) == crate::net::route::RTN_LOCAL
        })
        .map(IpAddress::Ipv4);
    let sent = Cell::new(false);
    let mut probe = PacketBuffer::try_with_capacity(86)?;
    let next_hop = bridge.smol_iface().lock().probe_ethernet_next_hop(
        crate::time::Instant::now().into(),
        PreparedFrameTxToken {
            frame: &mut probe,
            prepared: &sent,
        },
        next_hop,
        permanent,
        arp_source,
    );
    let probe_sent = sent.get();
    if probe_sent {
        bridge.raw_transmit_owned(probe)?;
    }
    match next_hop {
        Ok(mac) => Ok(mac),
        Err(IpPacketDispatchError::NeighborPending { retry_at }) => Err(RouteSendError::RetryAt {
            retry_at,
            probe_sent,
        }),
        Err(IpPacketDispatchError::Exhausted) => Err(SystemError::EAGAIN_OR_EWOULDBLOCK.into()),
        Err(_) => Err(SystemError::ENETUNREACH.into()),
    }
}

fn transmit_ip_frames(
    port: &BridgePort,
    bridge: &BridgeIface,
    frame: &IpFrame<'_>,
    reassembled: Option<crate::net::forward_mtu::ReassembledForwardInfo>,
) {
    use crate::driver::net::local_output::{ipv4_fragment, ipv6_fragment, ipv6_fragment_boundary};

    let mtu = port.bridge_enable.mtu();
    let largest = reassembled.map_or(mtu, |info| info.max_original_fragment_len);
    if frame.ip().len() <= mtu.min(largest) {
        port.transmit(frame.bytes());
        return;
    }
    if largest > mtu {
        return;
    }
    let Some(packet_version) = IpVersion::of_packet(frame.ip()).ok() else {
        return;
    };
    let (payload_len, ipv6_id, fragment_mtu) = match packet_version {
        IpVersion::Ipv4 => {
            let packet = Ipv4Packet::new_unchecked(frame.ip());
            if reassembled.is_none() && packet.dont_frag() {
                return;
            }
            (packet.payload().len(), None, largest)
        }
        IpVersion::Ipv6 => {
            if reassembled.is_none() || mtu < 1280 {
                return;
            }
            let Ok((header, _)) = ipv6_fragment_boundary(frame.ip()) else {
                return;
            };
            let Some(netns) = bridge.net_namespace() else {
                return;
            };
            (
                frame.ip().len() - header,
                Some(netns.next_ipv6_fragment_identification()),
                largest.max(1280),
            )
        }
    };
    let mut offset = 0;
    while offset < payload_len {
        let next = match ipv6_id {
            Some(id) => ipv6_fragment(frame.ip(), fragment_mtu, offset, id),
            None => ipv4_fragment(frame.ip(), fragment_mtu, offset),
        };
        let Ok((ip, chunk)) = next else {
            return;
        };
        let Ok(mut ethernet) = PacketBuffer::try_with_capacity(14 + ip.len()) else {
            return;
        };
        if ethernet.extend_from_slice(&frame.bytes()[..14]).is_err()
            || ethernet.extend_from_slice(&ip).is_err()
        {
            return;
        }
        port.transmit(&ethernet);
        offset += chunk;
    }
}
