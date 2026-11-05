use super::*;

#[cfg(all(feature = "alloc", feature = "medium-ethernet"))]
#[rstest]
#[case(Some(false), (false, true, false), ArpOperation::Request, true, (false, false))]
#[case(Some(false), (true, true, false), ArpOperation::Request, true, (false, true))]
#[case(Some(false), (false, false, false), ArpOperation::Request, true, (false, true))]
#[case(Some(false), (true, false, false), ArpOperation::Request, true, (false, true))]
#[case(Some(true), (false, false, false), ArpOperation::Request, true, (true, true))]
#[case(Some(true), (false, true, false), ArpOperation::Request, true, (true, false))]
#[case(Some(true), (false, true, true), ArpOperation::Request, true, (true, false))]
#[case(Some(true), (false, false, false), ArpOperation::Reply, true, (false, true))]
#[case(Some(false), (true, false, false), ArpOperation::Reply, true, (false, true))]
#[case(Some(true), (true, false, false), ArpOperation::Unknown(3), true, (false, false))]
#[case(Some(true), (true, false, false), ArpOperation::Request, false, (false, false))]
#[case(None, (false, false, false), ArpOperation::Request, true, (true, true))]
#[case(None, (false, true, false), ArpOperation::Request, true, (false, false))]
#[case(None, (true, true, false), ArpOperation::Request, true, (true, true))]
#[case(None, (true, true, true), ArpOperation::Request, true, (false, false))]
fn arp_reply_ownership_preserves_learning(
    #[case] ownership: Option<bool>,
    #[case] interface: (bool, bool, bool),
    #[case] operation: ArpOperation,
    #[case] valid_mac: bool,
    #[case] expected: (bool, bool),
) {
    let (any_ip, foreign_target, no_address) = interface;
    let (reply, learn) = expected;
    struct Ownership(Option<bool>, Ipv4Address, Ipv4Address);
    impl IpIngressFilter for Ownership {
        fn arp_reply_allowed(&self, source: Ipv4Address, target: Ipv4Address) -> Option<bool> {
            assert_eq!(source, self.1);
            assert_eq!(target, self.2);
            self.0
        }

        fn pre_routing(
            &mut self,
            _: &mut IngressPacket<'_>,
            _: PacketMeta,
            _: HardwareAddress,
        ) -> PreRoutingVerdict {
            panic!("ARP must not enter IP hooks")
        }
    }

    let (mut iface, mut sockets, _) = setup(Medium::Ethernet);
    iface.set_any_ip(any_ip);
    if no_address {
        iface.update_ip_addrs(|addresses| addresses.clear());
    }
    let target = if foreign_target {
        Ipv4Address::new(192, 168, 1, 3)
    } else {
        Ipv4Address::new(127, 0, 0, 1)
    };
    let source = if foreign_target {
        Ipv4Address::new(192, 168, 1, 2)
    } else {
        Ipv4Address::new(127, 0, 0, 2)
    };
    if any_ip && foreign_target && !no_address {
        iface.update_ip_addrs(|addresses| {
            addresses.clear();
            addresses.push(IpCidr::new(source.into(), 24)).unwrap();
        });
    }
    let source_mac = if valid_mac {
        EthernetAddress([0x52, 0x54, 0, 0, 0, 1])
    } else {
        EthernetAddress::BROADCAST
    };
    let arp = ArpRepr::EthernetIpv4 {
        operation,
        source_hardware_addr: source_mac,
        source_protocol_addr: source,
        target_hardware_addr: EthernetAddress::default(),
        target_protocol_addr: target,
    };
    let mut bytes = [0u8; 42];
    let mut frame = EthernetFrame::new_unchecked(&mut bytes[..]);
    frame.set_dst_addr(EthernetAddress::BROADCAST);
    frame.set_src_addr(source_mac);
    frame.set_ethertype(EthernetProtocol::Arp);
    arp.emit(&mut ArpPacket::new_unchecked(frame.payload_mut()));
    let mut scratch = crate::storage::PacketBytes::new();
    let mut filter = Ownership(ownership, source, target);
    let result = iface.inner.process_ethernet_filtered(
        &mut sockets,
        PacketMeta::default(),
        &bytes,
        &mut iface.fragments,
        &mut scratch,
        &mut filter,
    );
    assert_eq!(result.is_some(), reply);
    if reply {
        assert_eq!(
            result,
            Some(EthernetPacket::Arp(ArpRepr::EthernetIpv4 {
                operation: ArpOperation::Reply,
                source_hardware_addr: iface.inner.hardware_addr.ethernet_or_panic(),
                source_protocol_addr: target,
                target_hardware_addr: source_mac,
                target_protocol_addr: source,
            }))
        );
    }
    assert_eq!(
        iface
            .inner
            .neighbor_cache
            .lookup(&source.into(), Instant::ZERO)
            .found(),
        learn
    );
}

#[cfg(all(feature = "alloc", feature = "medium-ethernet"))]
#[rstest]
#[case(Some(true), ArpOperation::Request, true)]
#[case(Some(false), ArpOperation::Request, false)]
#[case(None, ArpOperation::Request, false)]
#[case(Some(true), ArpOperation::Reply, false)]
fn arp_local_dad_does_not_learn_zero(
    #[case] ownership: Option<bool>,
    #[case] operation: ArpOperation,
    #[case] reply: bool,
) {
    struct Ownership(Option<bool>);
    impl IpIngressFilter for Ownership {
        fn arp_reply_allowed(&self, source: Ipv4Address, target: Ipv4Address) -> Option<bool> {
            assert_eq!(source, Ipv4Address::UNSPECIFIED);
            assert_eq!(target, Ipv4Address::new(127, 0, 0, 1));
            self.0
        }
        fn pre_routing(
            &mut self,
            _: &mut IngressPacket<'_>,
            _: PacketMeta,
            _: HardwareAddress,
        ) -> PreRoutingVerdict {
            panic!("ARP must not enter IP hooks")
        }
    }
    let (mut iface, mut sockets, _) = setup(Medium::Ethernet);
    iface.set_any_ip(true);
    let arp = ArpRepr::EthernetIpv4 {
        operation,
        source_hardware_addr: EthernetAddress([0x52, 0x54, 0, 0, 0, 1]),
        source_protocol_addr: Ipv4Address::UNSPECIFIED,
        target_hardware_addr: EthernetAddress::default(),
        target_protocol_addr: Ipv4Address::new(127, 0, 0, 1),
    };
    let mut bytes = [0u8; 42];
    let mut frame = EthernetFrame::new_unchecked(&mut bytes[..]);
    frame.set_dst_addr(EthernetAddress::BROADCAST);
    frame.set_src_addr(EthernetAddress([0x52, 0x54, 0, 0, 0, 1]));
    frame.set_ethertype(EthernetProtocol::Arp);
    arp.emit(&mut ArpPacket::new_unchecked(frame.payload_mut()));
    let mut scratch = crate::storage::PacketBytes::new();
    let mut filter = Ownership(ownership);
    assert_eq!(
        iface
            .inner
            .process_ethernet_filtered(
                &mut sockets,
                PacketMeta::default(),
                &bytes,
                &mut iface.fragments,
                &mut scratch,
                &mut filter,
            )
            .is_some(),
        reply
    );
    assert_eq!(iface.inner.neighbor_cache.iter().count(), 0);
}

#[cfg(all(feature = "medium-ip", feature = "socket-udp"))]
#[test]
fn changed_output_policy_keeps_later_socket_packet_queued() {
    use crate::phy::{Device, DeviceCapabilities};
    use crate::socket::udp;
    use core::cell::Cell;

    struct ChangingPolicyDevice {
        inner: crate::tests::TestingDevice,
        current: Cell<bool>,
    }

    impl Device for ChangingPolicyDevice {
        type RxToken<'a> = crate::tests::RxToken;
        type TxToken<'a> = crate::tests::TxToken<'a>;

        fn receive(
            &mut self,
            timestamp: Instant,
        ) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
            self.inner.receive(timestamp)
        }

        fn transmit(&mut self, timestamp: Instant) -> Option<Self::TxToken<'_>> {
            self.current.set(false);
            self.inner.transmit(timestamp)
        }

        fn capabilities(&self) -> DeviceCapabilities {
            self.inner.capabilities()
        }

        fn policy_current(&self) -> bool {
            self.current.get()
        }
    }

    let (mut iface, mut sockets, inner) = setup(Medium::Ip);
    let mut handles = Vec::new();
    for port in [10001, 10002] {
        let rx = udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY], vec![0; 8]);
        let tx = udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY], vec![0; 8]);
        let mut socket = udp::Socket::new(rx, tx);
        socket.bind(port).unwrap();
        socket
            .send_slice(
                b"one",
                IpEndpoint::new(IpAddress::v4(192, 168, 1, 2), 20000),
            )
            .unwrap();
        handles.push(sockets.add(socket));
    }
    let mut device = ChangingPolicyDevice {
        inner,
        current: Cell::new(true),
    };

    iface.poll_egress(Instant::ZERO, &mut device, &mut sockets);
    assert_eq!(sockets.get_mut::<udp::Socket>(handles[0]).send_queue(), 0);
    assert_eq!(sockets.get_mut::<udp::Socket>(handles[1]).send_queue(), 3);
    device.current.set(true);
    iface.poll_egress(Instant::ZERO, &mut device, &mut sockets);
    assert_eq!(sockets.get_mut::<udp::Socket>(handles[1]).send_queue(), 0);
}

#[cfg(all(feature = "alloc", feature = "medium-ethernet", feature = "socket-raw"))]
#[test]
fn deferred_ipv4_output_serializes_before_neighbor_and_fragmentation() {
    use core::cell::RefCell;
    use std::rc::Rc;

    struct DeferredToken(Rc<RefCell<Vec<u8>>>);

    impl TxToken for DeferredToken {
        fn deferred_ip_output(&self, _: IpVersion) -> bool {
            true
        }

        fn consume_full_ip<F>(
            self,
            len: usize,
            _: PacketMeta,
            class: crate::phy::IpOutputClass,
            ipv4_fragment_ident: Option<u16>,
            emit: F,
        ) -> Result<(), crate::phy::IpOutputError>
        where
            F: FnOnce(&mut [u8]),
        {
            assert_eq!(class, crate::phy::IpOutputClass::Ordinary);
            #[cfg(feature = "proto-ipv4-fragmentation")]
            assert!(ipv4_fragment_ident.is_some());
            #[cfg(not(feature = "proto-ipv4-fragmentation"))]
            assert!(ipv4_fragment_ident.is_none());
            let mut bytes = vec![0; len];
            emit(&mut bytes);
            *self.0.borrow_mut() = bytes;
            Ok(())
        }

        fn consume<R, F>(self, _: usize, _: F) -> R
        where
            F: FnOnce(&mut [u8]) -> R,
        {
            panic!("deferred output must not use link-layer transmission")
        }
    }

    let (mut iface, _, _) = setup(Medium::Ethernet);
    let payload = vec![0xa5; iface.inner.ip_mtu() + 256];
    let packet = Packet::new_ipv4(
        Ipv4Repr {
            src_addr: Ipv4Address::new(192, 0, 2, 1),
            dst_addr: Ipv4Address::new(198, 51, 100, 1),
            next_header: IpProtocol::Udp,
            payload_len: payload.len(),
            hop_limit: 64,
        },
        IpPayload::Raw(&payload),
    );
    let emitted = Rc::new(RefCell::new(Vec::new()));
    iface
        .inner
        .dispatch_ip(
            DeferredToken(emitted.clone()),
            PacketMeta::default(),
            packet,
            &mut iface.fragmenter,
        )
        .unwrap();
    let output = emitted.borrow();
    let ipv4 = Ipv4Packet::new_checked(&output[..]).unwrap();
    assert_eq!(ipv4.total_len() as usize, output.len());
    assert!(ipv4.dont_frag());
    assert_eq!(ipv4.ident(), 0);
    assert_eq!(ipv4.dst_addr(), Ipv4Address::new(198, 51, 100, 1));
    assert_eq!(ipv4.payload(), &payload[..]);
    #[cfg(feature = "proto-ipv4-fragmentation")]
    assert_eq!(iface.fragmenter.packet_len, 0);
}

#[cfg(feature = "alloc")]
struct RewriteIpv4Destination {
    calls: usize,
}

#[cfg(feature = "alloc")]
impl IpIngressFilter for RewriteIpv4Destination {
    fn pre_routing(
        &mut self,
        packet: &mut IngressPacket<'_>,
        _: PacketMeta,
        _: HardwareAddress,
    ) -> PreRoutingVerdict {
        self.calls += 1;
        let incoming = Ipv4Packet::new_checked(packet.bytes()).unwrap();
        assert_eq!(incoming.hop_limit(), 64);
        assert_eq!(incoming.total_len() as usize, packet.bytes().len());
        let mut ipv4 = Ipv4Packet::new_unchecked(packet.writable().unwrap());
        ipv4.set_dst_addr(Ipv4Address::new(127, 0, 0, 1));
        ipv4.fill_checksum();
        PreRoutingVerdict::Pass
    }
}

#[cfg(all(feature = "alloc", feature = "medium-ip"))]
fn icmp_request_for_rewrite() -> Vec<u8> {
    use crate::wire::{Icmpv4Packet, Icmpv4Repr};

    let payload = [1, 2, 3, 4];
    let icmp = Icmpv4Repr::EchoRequest {
        ident: 42,
        seq_no: 1,
        data: &payload,
    };
    let repr = Ipv4Repr {
        src_addr: Ipv4Address::new(127, 0, 0, 2),
        dst_addr: Ipv4Address::new(127, 0, 0, 3),
        next_header: IpProtocol::Icmp,
        payload_len: icmp.buffer_len(),
        hop_limit: 64,
    };
    let mut bytes = vec![0; repr.buffer_len() + icmp.buffer_len()];
    repr.emit(
        &mut Ipv4Packet::new_unchecked(&mut bytes),
        &ChecksumCapabilities::default(),
    );
    icmp.emit(
        &mut Icmpv4Packet::new_unchecked(&mut bytes[repr.buffer_len()..]),
        &ChecksumCapabilities::default(),
    );
    bytes
}

#[cfg(all(feature = "alloc", feature = "medium-ip"))]
struct TransferIpv4Packet {
    forwarded: Vec<Vec<u8>>,
}

#[cfg(all(feature = "alloc", feature = "medium-ip"))]
impl IpIngressFilter for TransferIpv4Packet {
    fn defragment_ipv4(&self) -> bool {
        false
    }

    fn pre_routing(
        &mut self,
        _: &mut IngressPacket<'_>,
        _: PacketMeta,
        _: HardwareAddress,
    ) -> PreRoutingVerdict {
        PreRoutingVerdict::Pass
    }

    fn route_input(
        &mut self,
        packet: &mut RoutedIngressPacket<'_, '_>,
        _: PacketMeta,
        _: HardwareAddress,
    ) -> RouteInputVerdict {
        self.forwarded.push(packet.take_owned().unwrap().to_vec());
        RouteInputVerdict::Forward
    }

    fn route_fragment(
        &mut self,
        packet: &mut RoutedIngressPacket<'_, '_>,
        meta: PacketMeta,
        source_hardware_addr: HardwareAddress,
    ) -> RouteInputVerdict {
        self.route_input(packet, meta, source_hardware_addr)
    }
}

#[cfg(all(feature = "alloc", feature = "medium-ip"))]
#[test]
fn forwarded_ipv4_packet_survives_rx_token_without_local_reply() {
    let (mut iface, mut sockets, mut device) = setup(Medium::Ip);
    let mut bytes = icmp_request_for_rewrite();
    let mut ipv4 = Ipv4Packet::new_unchecked(&mut bytes);
    ipv4.set_dst_addr(Ipv4Address::new(127, 0, 0, 1));
    ipv4.fill_checksum();
    device.rx_queue.push_back(bytes);
    let mut filter = TransferIpv4Packet {
        forwarded: Vec::new(),
    };
    assert_eq!(
        iface.poll_filtered(Instant::ZERO, &mut device, &mut sockets, &mut filter),
        PollResult::SocketStateChanged
    );
    assert!(device.tx_queue.is_empty());
    assert_eq!(filter.forwarded.len(), 1);
    let packet = Ipv4Packet::new_checked(&filter.forwarded[0][..]).unwrap();
    assert_eq!(packet.dst_addr(), Ipv4Address::new(127, 0, 0, 1));
}

#[cfg(all(
    feature = "alloc",
    feature = "medium-ip",
    feature = "proto-ipv4-fragmentation"
))]
#[test]
fn forwarded_ipv4_fragments_do_not_use_local_reassembly_slots() {
    let (mut iface, mut sockets, _) = setup(Medium::Ip);
    let mut scratch = crate::storage::PacketBytes::new();
    let mut filter = TransferIpv4Packet {
        forwarded: Vec::new(),
    };

    for (offset, len, more_fragments) in [(0, 1480, true), (1480, 520, false)] {
        let repr = Ipv4Repr {
            src_addr: Ipv4Address::new(192, 0, 2, 1),
            dst_addr: Ipv4Address::new(198, 51, 100, 1),
            next_header: IpProtocol::Udp,
            payload_len: len,
            hop_limit: 64,
        };
        let mut bytes = vec![0; repr.buffer_len() + len];
        let mut fragment = Ipv4Packet::new_unchecked(&mut bytes[..]);
        repr.emit(&mut fragment, &ChecksumCapabilities::default());
        fragment.set_ident(123);
        fragment.set_frag_offset(offset);
        fragment.set_more_frags(more_fragments);
        fragment.fill_checksum();
        let fragment = Ipv4Packet::new_checked(&bytes[..]).unwrap();
        assert!(iface
            .inner
            .process_ipv4_filtered(
                &mut sockets,
                PacketMeta::default(),
                HardwareAddress::Ip,
                &fragment,
                &mut iface.fragments,
                &mut scratch,
                &mut filter,
            )
            .is_none());
    }

    assert_eq!(filter.forwarded.len(), 2);
    for (packet, (offset, len, more_fragments)) in filter
        .forwarded
        .iter()
        .zip([(0, 1480, true), (1480, 520, false)])
    {
        let fragment = Ipv4Packet::new_checked(packet.as_slice()).unwrap();
        assert_eq!(fragment.frag_offset(), offset);
        assert_eq!(fragment.payload().len(), len);
        assert_eq!(fragment.more_frags(), more_fragments);
    }
}

#[cfg(all(
    feature = "alloc",
    feature = "medium-ip",
    feature = "proto-ipv4-fragmentation"
))]
#[test]
fn stateless_fragment_policy_can_drop_local_and_transit_fragments() {
    struct DropFragments(usize);

    impl IpIngressFilter for DropFragments {
        fn defragment_ipv4(&self) -> bool {
            false
        }

        fn pre_routing(
            &mut self,
            _: &mut IngressPacket<'_>,
            _: PacketMeta,
            _: HardwareAddress,
        ) -> PreRoutingVerdict {
            panic!("dropped transit fragments must not reach local reassembly")
        }

        fn route_fragment(
            &mut self,
            _: &mut RoutedIngressPacket<'_, '_>,
            _: PacketMeta,
            _: HardwareAddress,
        ) -> RouteInputVerdict {
            self.0 += 1;
            RouteInputVerdict::Drop
        }
    }

    let (mut iface, mut sockets, _) = setup(Medium::Ip);
    let mut scratch = crate::storage::PacketBytes::new();
    let mut filter = DropFragments(0);
    for destination in [
        Ipv4Address::new(198, 51, 100, 1),
        Ipv4Address::new(127, 0, 0, 1),
    ] {
        for (offset, len, more_fragments) in [(0, 1480, true), (1480, 520, false)] {
            let repr = Ipv4Repr {
                src_addr: Ipv4Address::new(127, 0, 0, 2),
                dst_addr: destination,
                next_header: IpProtocol::Udp,
                payload_len: len,
                hop_limit: 64,
            };
            let mut bytes = vec![0; repr.buffer_len() + len];
            let mut fragment = Ipv4Packet::new_unchecked(&mut bytes[..]);
            repr.emit(&mut fragment, &ChecksumCapabilities::default());
            fragment.set_ident(124);
            fragment.set_frag_offset(offset);
            fragment.set_more_frags(more_fragments);
            fragment.fill_checksum();
            let fragment = Ipv4Packet::new_checked(&bytes[..]).unwrap();
            assert!(iface
                .inner
                .process_ipv4_filtered(
                    &mut sockets,
                    PacketMeta::default(),
                    HardwareAddress::Ip,
                    &fragment,
                    &mut iface.fragments,
                    &mut scratch,
                    &mut filter,
                )
                .is_none());
        }
    }
    assert_eq!(filter.0, 4);
}

#[cfg(all(
    feature = "alloc",
    feature = "medium-ip",
    feature = "proto-ipv4-fragmentation"
))]
#[test]
fn local_ipv4_fragments_run_policy_once_per_fragment() {
    struct AcceptFragments {
        fragments: usize,
        assembled: usize,
        mark: core::cell::Cell<u32>,
    }

    impl IpIngressFilter for AcceptFragments {
        fn defragment_ipv4(&self) -> bool {
            false
        }

        fn packet_mark(&self) -> u32 {
            self.mark.get()
        }

        fn restore_packet_mark(&self, mark: u32) {
            self.mark.set(mark);
        }

        fn pre_routing(
            &mut self,
            _: &mut IngressPacket<'_>,
            _: PacketMeta,
            _: HardwareAddress,
        ) -> PreRoutingVerdict {
            panic!("local reassembly must not repeat a fragment verdict")
        }

        fn route_fragment(
            &mut self,
            packet: &mut RoutedIngressPacket<'_, '_>,
            _: PacketMeta,
            _: HardwareAddress,
        ) -> RouteInputVerdict {
            self.fragments += 1;
            let offset = Ipv4Packet::new_checked(packet.bytes())
                .unwrap()
                .frag_offset();
            self.mark.set(if offset == 0 { 0x1234 } else { 0x5678 });
            RouteInputVerdict::Pass
        }

        fn route_input(
            &mut self,
            _: &mut RoutedIngressPacket<'_, '_>,
            _: PacketMeta,
            _: HardwareAddress,
        ) -> RouteInputVerdict {
            self.assembled += 1;
            assert_eq!(self.mark.get(), 0x1234);
            RouteInputVerdict::Pass
        }
    }

    let (mut iface, mut sockets, _) = setup(Medium::Ip);
    let mut scratch = crate::storage::PacketBytes::new();
    let mut filter = AcceptFragments {
        fragments: 0,
        assembled: 0,
        mark: core::cell::Cell::new(0),
    };
    for offset in [0, 16] {
        let repr = Ipv4Repr {
            src_addr: Ipv4Address::new(127, 0, 0, 2),
            dst_addr: Ipv4Address::new(127, 0, 0, 1),
            next_header: IpProtocol::Udp,
            payload_len: 16,
            hop_limit: 64,
        };
        let mut bytes = vec![0; repr.buffer_len() + 16];
        let mut fragment = Ipv4Packet::new_unchecked(&mut bytes[..]);
        repr.emit(&mut fragment, &ChecksumCapabilities::default());
        fragment.set_ident(125);
        fragment.set_frag_offset(offset);
        fragment.set_more_frags(offset == 0);
        fragment.fill_checksum();
        let fragment = Ipv4Packet::new_checked(&bytes[..]).unwrap();
        let _ = iface.inner.process_ipv4_filtered(
            &mut sockets,
            PacketMeta::default(),
            HardwareAddress::Ip,
            &fragment,
            &mut iface.fragments,
            &mut scratch,
            &mut filter,
        );
    }
    assert_eq!(filter.fragments, 2);
    assert_eq!(filter.assembled, 1);
}

#[cfg(all(feature = "alloc", feature = "medium-ip", feature = "packetmeta-id"))]
#[test]
fn ipv4_pre_routing_receives_rx_token_metadata() {
    struct CaptureMeta(Option<PacketMeta>);

    impl IpIngressFilter for CaptureMeta {
        fn pre_routing(
            &mut self,
            _: &mut IngressPacket<'_>,
            meta: PacketMeta,
            _: HardwareAddress,
        ) -> PreRoutingVerdict {
            self.0 = Some(meta);
            PreRoutingVerdict::Drop
        }
    }

    let (mut iface, mut sockets, mut device) = setup(Medium::Ip);
    device.rx_queue.push_back(icmp_request_for_rewrite());
    let mut filter = CaptureMeta(None);
    let meta = PacketMeta { id: 71 };
    device.rx_meta = meta;
    assert_eq!(
        iface.poll_ingress_single_filtered(Instant::ZERO, &mut device, &mut sockets, &mut filter),
        PollIngressSingleResult::SocketStateChanged
    );
    assert_eq!(filter.0, Some(meta));
}

#[cfg(all(feature = "alloc", feature = "medium-ip"))]
#[test]
fn full_poll_rechecks_filter_interest_for_each_ipv4_packet() {
    use core::cell::Cell;

    struct ActivateOnSecond {
        checks: Cell<usize>,
        starts: usize,
        calls: usize,
    }

    impl IpIngressFilter for ActivateOnSecond {
        fn begin_packet(&mut self, _: PacketMeta) {
            self.starts += 1;
        }

        fn applies_to(&self, version: IpVersion) -> bool {
            assert_eq!(version, IpVersion::Ipv4);
            let checks = self.checks.get();
            self.checks.set(checks + 1);
            checks != 0
        }

        fn pre_routing(
            &mut self,
            _: &mut IngressPacket<'_>,
            _: PacketMeta,
            _: HardwareAddress,
        ) -> PreRoutingVerdict {
            self.calls += 1;
            PreRoutingVerdict::Drop
        }
    }

    let (mut iface, mut sockets, mut device) = setup(Medium::Ip);
    device.rx_queue.push_back(icmp_request_for_rewrite());
    device.rx_queue.push_back(icmp_request_for_rewrite());
    device.rx_queue.push_back(alloc::vec::Vec::new());
    let mut filter = ActivateOnSecond {
        checks: Cell::new(0),
        starts: 0,
        calls: 0,
    };
    iface.poll_filtered(Instant::ZERO, &mut device, &mut sockets, &mut filter);
    assert_eq!(filter.checks.get(), 2);
    assert_eq!(filter.starts, 3);
    assert_eq!(filter.calls, 1);
}

#[cfg(all(feature = "alloc", feature = "medium-ip"))]
#[test]
fn paused_filter_keeps_next_rx_token_for_a_fresh_policy_view() {
    use core::cell::Cell;

    struct PauseAfterPacket {
        paused: Cell<bool>,
        calls: usize,
    }

    impl IpIngressFilter for PauseAfterPacket {
        fn continue_ingress_poll(&self) -> bool {
            !self.paused.get()
        }

        fn pre_routing(
            &mut self,
            _: &mut IngressPacket<'_>,
            _: PacketMeta,
            _: HardwareAddress,
        ) -> PreRoutingVerdict {
            self.calls += 1;
            self.paused.set(true);
            PreRoutingVerdict::Drop
        }
    }

    let (mut iface, mut sockets, mut device) = setup(Medium::Ip);
    device.rx_queue.push_back(icmp_request_for_rewrite());
    device.rx_queue.push_back(icmp_request_for_rewrite());
    let mut filter = PauseAfterPacket {
        paused: Cell::new(false),
        calls: 0,
    };
    iface.poll_filtered(Instant::ZERO, &mut device, &mut sockets, &mut filter);
    assert_eq!(filter.calls, 1);
    assert_eq!(device.rx_queue.len(), 1);

    assert_eq!(
        iface.poll_ingress_single_filtered(Instant::ZERO, &mut device, &mut sockets, &mut filter),
        PollIngressSingleResult::None
    );
    assert_eq!(device.rx_queue.len(), 1);

    filter.paused.set(false);
    iface.poll_filtered(Instant::ZERO, &mut device, &mut sockets, &mut filter);
    assert_eq!(filter.calls, 2);
    assert!(device.rx_queue.is_empty());
}

#[cfg(all(feature = "alloc", feature = "medium-ip"))]
#[test]
fn rewritten_ipv4_packet_keeps_reply_borrow_alive() {
    use crate::wire::Icmpv4Repr;

    let (mut iface, mut sockets, _) = setup(Medium::Ip);
    let payload = [1, 2, 3, 4];
    let src = Ipv4Address::new(127, 0, 0, 2);
    let bytes = icmp_request_for_rewrite();
    let packet = Ipv4Packet::new_checked(&bytes[..]).unwrap();
    let mut scratch = crate::storage::PacketBytes::new();
    let mut filter = RewriteIpv4Destination { calls: 0 };
    let response = iface.inner.process_ipv4_filtered(
        &mut sockets,
        PacketMeta::default(),
        HardwareAddress::Ip,
        &packet,
        &mut iface.fragments,
        &mut scratch,
        &mut filter,
    );
    let expected = Packet::new_ipv4(
        Ipv4Repr {
            src_addr: Ipv4Address::new(127, 0, 0, 1),
            dst_addr: src,
            next_header: IpProtocol::Icmp,
            payload_len: 8 + payload.len(),
            hop_limit: 64,
        },
        IpPayload::Icmpv4(Icmpv4Repr::EchoReply {
            ident: 42,
            seq_no: 1,
            data: &payload,
        }),
    );
    assert_eq!(response, Some(expected));
    assert_eq!(filter.calls, 1);
}

#[cfg(all(feature = "alloc", feature = "medium-ip"))]
#[test]
fn local_input_ipv4_rewrite_is_validated_before_transport_reply() {
    use crate::wire::Icmpv4Repr;

    struct RewriteSource {
        calls: usize,
        repair_checksum: bool,
    }

    impl IpIngressFilter for RewriteSource {
        fn pre_routing(
            &mut self,
            _: &mut IngressPacket<'_>,
            _: PacketMeta,
            _: HardwareAddress,
        ) -> PreRoutingVerdict {
            PreRoutingVerdict::Pass
        }

        fn local_input(
            &mut self,
            packet: &mut IngressPacket<'_>,
            _: PacketMeta,
            _: HardwareAddress,
            protocol: IpProtocol,
            transport_offset: usize,
        ) -> LocalInputVerdict {
            self.calls += 1;
            assert_eq!(protocol, IpProtocol::Icmp);
            assert_eq!(transport_offset, 20);
            let mut ipv4 = Ipv4Packet::new_unchecked(packet.writable().unwrap());
            ipv4.set_src_addr(Ipv4Address::new(127, 0, 0, 9));
            if self.repair_checksum {
                ipv4.fill_checksum();
            }
            LocalInputVerdict::Pass
        }
    }

    let (mut iface, mut sockets, _) = setup(Medium::Ip);
    let mut bytes = icmp_request_for_rewrite();
    let mut ip = Ipv4Packet::new_unchecked(&mut bytes[..]);
    ip.set_dst_addr(Ipv4Address::new(127, 0, 0, 1));
    ip.fill_checksum();
    let original = bytes.clone();
    let packet = Ipv4Packet::new_checked(&bytes[..]).unwrap();
    let mut scratch = crate::storage::PacketBytes::new();
    let mut filter = RewriteSource {
        calls: 0,
        repair_checksum: true,
    };
    let reply = iface.inner.process_ipv4_filtered(
        &mut sockets,
        PacketMeta::default(),
        HardwareAddress::Ip,
        &packet,
        &mut iface.fragments,
        &mut scratch,
        &mut filter,
    );
    let expected = Packet::new_ipv4(
        Ipv4Repr {
            src_addr: Ipv4Address::new(127, 0, 0, 1),
            dst_addr: Ipv4Address::new(127, 0, 0, 9),
            next_header: IpProtocol::Icmp,
            payload_len: 12,
            hop_limit: 64,
        },
        IpPayload::Icmpv4(Icmpv4Repr::EchoReply {
            ident: 42,
            seq_no: 1,
            data: &[1, 2, 3, 4],
        }),
    );
    assert_eq!(reply, Some(expected));
    assert_eq!(filter.calls, 1);
    assert_eq!(bytes, original);
    drop(reply);
    assert_eq!(scratch[12..16], [127, 0, 0, 9]);

    let mut malformed = RewriteSource {
        calls: 0,
        repair_checksum: false,
    };
    assert!(iface
        .inner
        .process_ipv4_filtered(
            &mut sockets,
            PacketMeta::default(),
            HardwareAddress::Ip,
            &packet,
            &mut iface.fragments,
            &mut scratch,
            &mut malformed,
        )
        .is_none());
    assert_eq!(malformed.calls, 1);
}

#[cfg(all(feature = "alloc", feature = "medium-ip"))]
#[test]
fn local_input_ipv4_cannot_rewrite_destination_to_nonlocal() {
    struct RewriteDestination;

    impl IpIngressFilter for RewriteDestination {
        fn pre_routing(
            &mut self,
            _: &mut IngressPacket<'_>,
            _: PacketMeta,
            _: HardwareAddress,
        ) -> PreRoutingVerdict {
            PreRoutingVerdict::Pass
        }

        fn local_input(
            &mut self,
            packet: &mut IngressPacket<'_>,
            _: PacketMeta,
            _: HardwareAddress,
            _: IpProtocol,
            _: usize,
        ) -> LocalInputVerdict {
            let mut ipv4 = Ipv4Packet::new_unchecked(packet.writable().unwrap());
            ipv4.set_dst_addr(Ipv4Address::new(192, 0, 2, 9));
            ipv4.fill_checksum();
            LocalInputVerdict::Pass
        }
    }

    let (mut iface, mut sockets, _) = setup(Medium::Ip);
    let mut bytes = icmp_request_for_rewrite();
    let mut ip = Ipv4Packet::new_unchecked(&mut bytes[..]);
    ip.set_dst_addr(Ipv4Address::new(127, 0, 0, 1));
    ip.fill_checksum();
    let packet = Ipv4Packet::new_checked(&bytes[..]).unwrap();
    let mut scratch = crate::storage::PacketBytes::new();
    assert!(iface
        .inner
        .process_ipv4_filtered(
            &mut sockets,
            PacketMeta::default(),
            HardwareAddress::Ip,
            &packet,
            &mut iface.fragments,
            &mut scratch,
            &mut RewriteDestination,
        )
        .is_none());
}

#[cfg(all(feature = "alloc", feature = "medium-ip"))]
#[test]
fn normal_poll_ingress_applies_ipv4_rewrite_before_reply() {
    let (mut iface, mut sockets, mut device) = setup(Medium::Ip);
    let mut padded = icmp_request_for_rewrite();
    padded.extend_from_slice(&[0; 8]);
    device.rx_queue.push_back(padded);
    let mut filter = RewriteIpv4Destination { calls: 0 };
    assert_eq!(
        iface.poll_ingress_single_filtered(Instant::ZERO, &mut device, &mut sockets, &mut filter),
        PollIngressSingleResult::SocketStateChanged
    );
    let reply = device.tx_queue.pop_front().unwrap();
    let packet = Ipv4Packet::new_checked(&reply[..]).unwrap();
    assert_eq!(packet.src_addr(), Ipv4Address::new(127, 0, 0, 1));
    assert_eq!(packet.dst_addr(), Ipv4Address::new(127, 0, 0, 2));
    assert_eq!(packet.next_header(), IpProtocol::Icmp);
    assert_eq!(filter.calls, 1);
}

#[cfg(all(feature = "alloc", feature = "medium-ip"))]
#[test]
fn ipv4_source_policy_runs_after_pre_routing_decision() {
    let (mut iface, mut sockets, mut device) = setup(Medium::Ip);
    let mut bytes = icmp_request_for_rewrite();
    let mut ipv4 = Ipv4Packet::new_unchecked(&mut bytes);
    ipv4.set_src_addr(Ipv4Address::new(224, 0, 0, 1));
    ipv4.fill_checksum();
    device.rx_queue.push_back(bytes);
    let mut filter = RewriteIpv4Destination { calls: 0 };
    iface.poll_ingress_single_filtered(Instant::ZERO, &mut device, &mut sockets, &mut filter);
    assert_eq!(filter.calls, 1);
    assert!(device.tx_queue.is_empty());
}

#[cfg(all(feature = "alloc", feature = "medium-ip"))]
#[test]
fn ipv4_invalid_source_cannot_reach_route_input() {
    let (mut iface, mut sockets, mut device) = setup(Medium::Ip);
    let mut bytes = icmp_request_for_rewrite();
    let mut ipv4 = Ipv4Packet::new_unchecked(&mut bytes);
    ipv4.set_src_addr(Ipv4Address::new(224, 0, 0, 1));
    ipv4.fill_checksum();
    device.rx_queue.push_back(bytes);
    let mut filter = TransferIpv4Packet {
        forwarded: Vec::new(),
    };
    iface.poll_ingress_single_filtered(Instant::ZERO, &mut device, &mut sockets, &mut filter);
    assert!(filter.forwarded.is_empty());
}

#[cfg(all(feature = "proto-ipv4-fragmentation", feature = "medium-ip"))]
#[test]
fn unfiltered_nonunicast_fragment_does_not_consume_reassembly_slot() {
    let (mut iface, mut sockets, _) = setup(Medium::Ip);
    let repr = Ipv4Repr {
        src_addr: Ipv4Address::new(224, 0, 0, 1),
        dst_addr: Ipv4Address::new(127, 0, 0, 1),
        next_header: IpProtocol::Icmp,
        payload_len: 8,
        hop_limit: 64,
    };
    let mut bytes = vec![0; repr.buffer_len() + 8];
    let mut packet = Ipv4Packet::new_unchecked(&mut bytes[..]);
    repr.emit(&mut packet, &ChecksumCapabilities::default());
    packet.set_ident(123);
    packet.set_more_frags(true);
    packet.fill_checksum();
    let packet = Ipv4Packet::new_checked(&bytes[..]).unwrap();
    assert!(iface
        .inner
        .process_ipv4(
            &mut sockets,
            PacketMeta::default(),
            HardwareAddress::Ip,
            &packet,
            &mut iface.fragments,
        )
        .is_none());
    let assembler = iface
        .fragments
        .assembler
        .get(&FragKey::Ipv4(packet.get_key()), Instant::from_millis(1000))
        .unwrap();
    assert_eq!(assembler.classify_received_range(0, 8), FragmentRange::New);
}

#[cfg(all(feature = "alloc", feature = "medium-ip"))]
#[test]
fn full_poll_applies_ipv4_rewrite_to_every_ingress_packet() {
    let (mut iface, mut sockets, mut device) = setup(Medium::Ip);
    device.rx_queue.push_back(icmp_request_for_rewrite());
    device.rx_queue.push_back(icmp_request_for_rewrite());
    let mut filter = RewriteIpv4Destination { calls: 0 };
    assert_eq!(
        iface.poll_filtered(Instant::ZERO, &mut device, &mut sockets, &mut filter),
        PollResult::SocketStateChanged
    );
    assert_eq!(filter.calls, 2);
    for _ in 0..2 {
        let reply = device.tx_queue.pop_front().unwrap();
        let packet = Ipv4Packet::new_checked(&reply[..]).unwrap();
        assert_eq!(packet.src_addr(), Ipv4Address::new(127, 0, 0, 1));
    }
}

#[cfg(all(
    feature = "alloc",
    feature = "medium-ip",
    feature = "proto-ipv4-fragmentation"
))]
#[test]
fn filtered_ipv4_fragments_are_reassembled_before_rewrite() {
    let (mut iface, mut sockets, _) = setup(Medium::Ip);
    let original = icmp_request_for_rewrite();
    let datagram = &original[20..];
    let mut scratch = crate::storage::PacketBytes::new();
    let mut filter = RewriteIpv4Destination { calls: 0 };

    for (offset, payload, more_fragments) in [(0, &datagram[..8], true), (8, &datagram[8..], false)]
    {
        let repr = Ipv4Repr {
            src_addr: Ipv4Address::new(127, 0, 0, 2),
            dst_addr: Ipv4Address::new(127, 0, 0, 3),
            next_header: IpProtocol::Icmp,
            payload_len: payload.len(),
            hop_limit: if offset == 0 { 64 } else { 1 },
        };
        let mut bytes = vec![0; repr.buffer_len() + payload.len()];
        let mut packet = Ipv4Packet::new_unchecked(&mut bytes[..]);
        repr.emit(&mut packet, &ChecksumCapabilities::default());
        packet.set_ident(77);
        packet.set_frag_offset(offset);
        packet.set_more_frags(more_fragments);
        packet.fill_checksum();
        packet.payload_mut().copy_from_slice(payload);
        let packet = Ipv4Packet::new_checked(&bytes[..]).unwrap();
        let response = iface.inner.process_ipv4_filtered(
            &mut sockets,
            PacketMeta::default(),
            HardwareAddress::Ip,
            &packet,
            &mut iface.fragments,
            &mut scratch,
            &mut filter,
        );
        if more_fragments {
            assert!(response.is_none());
            assert_eq!(filter.calls, 0);
        } else {
            assert!(response.is_some());
            assert_eq!(filter.calls, 1);
            assert_eq!(
                response.unwrap().ip_repr().dst_addr(),
                Ipv4Address::new(127, 0, 0, 2).into()
            );
        }
    }
}

#[cfg(all(
    feature = "alloc",
    feature = "medium-ip",
    feature = "medium-ethernet",
    feature = "proto-ipv4-fragmentation"
))]
#[test]
fn filtered_ipv4_reassembly_reports_the_first_fragment_source_mac() {
    struct CaptureSource(Option<HardwareAddress>);
    impl IpIngressFilter for CaptureSource {
        fn pre_routing(
            &mut self,
            _: &mut IngressPacket<'_>,
            _: PacketMeta,
            source: HardwareAddress,
        ) -> PreRoutingVerdict {
            self.0 = Some(source);
            PreRoutingVerdict::Drop
        }
    }

    let (mut iface, mut sockets, _) = setup(Medium::Ip);
    let original = icmp_request_for_rewrite();
    let datagram = &original[20..];
    let first_mac = HardwareAddress::Ethernet(EthernetAddress([2, 0, 0, 0, 0, 1]));
    let final_mac = HardwareAddress::Ethernet(EthernetAddress([2, 0, 0, 0, 0, 2]));
    let mut scratch = crate::storage::PacketBytes::new();
    let mut filter = CaptureSource(None);

    for (offset, payload, more_fragments, source) in [
        (0, &datagram[..8], true, first_mac),
        (8, &datagram[8..], false, final_mac),
    ] {
        let repr = Ipv4Repr {
            src_addr: Ipv4Address::new(127, 0, 0, 2),
            dst_addr: Ipv4Address::new(127, 0, 0, 3),
            next_header: IpProtocol::Icmp,
            payload_len: payload.len(),
            hop_limit: 64,
        };
        let mut bytes = vec![0; repr.buffer_len() + payload.len()];
        let mut fragment = Ipv4Packet::new_unchecked(&mut bytes[..]);
        repr.emit(&mut fragment, &ChecksumCapabilities::default());
        fragment.set_ident(78);
        fragment.set_frag_offset(offset);
        fragment.set_more_frags(more_fragments);
        fragment.fill_checksum();
        fragment.payload_mut().copy_from_slice(payload);
        let fragment = Ipv4Packet::new_checked(&bytes[..]).unwrap();
        assert!(iface
            .inner
            .process_ipv4_filtered(
                &mut sockets,
                PacketMeta::default(),
                source,
                &fragment,
                &mut iface.fragments,
                &mut scratch,
                &mut filter,
            )
            .is_none());
    }
    assert_eq!(filter.0, Some(first_mac));
}

fn serialized_ipv4_packet(dst_addr: Ipv4Address) -> Vec<u8> {
    let payload = [0xde, 0xad, 0xbe, 0xef];
    let repr = Ipv4Repr {
        src_addr: Ipv4Address::new(192, 168, 1, 1),
        dst_addr,
        next_header: IpProtocol::Udp,
        payload_len: payload.len(),
        hop_limit: 64,
    };
    let mut bytes = vec![0; repr.buffer_len() + payload.len()];
    repr.emit(
        &mut Ipv4Packet::new_unchecked(&mut bytes),
        &ChecksumCapabilities::default(),
    );
    bytes[repr.buffer_len()..].copy_from_slice(&payload);
    bytes
}

#[derive(Clone)]
struct CapturingTxToken(std::rc::Rc<core::cell::RefCell<Vec<Vec<u8>>>>);

impl TxToken for CapturingTxToken {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let mut buffer = vec![0; len];
        let result = f(&mut buffer);
        self.0.borrow_mut().push(buffer);
        result
    }
}

#[cfg(feature = "medium-ethernet")]
#[rstest]
#[case::secondary_local(
    Ipv4Address::new(198, 18, 220, 1),
    Some(Ipv4Address::new(198, 18, 220, 1)),
    Ipv4Address::new(192, 0, 2, 254),
    Ipv4Address::new(198, 18, 220, 1)
)]
#[case::configured_but_not_namespace_local(
    Ipv4Address::new(198, 18, 220, 1),
    None,
    Ipv4Address::new(192, 0, 2, 254),
    Ipv4Address::new(192, 0, 2, 1)
)]
#[case::authorized_other_interface(
    Ipv4Address::new(198, 18, 215, 1),
    Some(Ipv4Address::new(198, 18, 215, 1)),
    Ipv4Address::new(198, 18, 220, 254),
    Ipv4Address::new(198, 18, 215, 1)
)]
#[case::forwarded_remote(
    Ipv4Address::new(203, 0, 113, 7),
    None,
    Ipv4Address::new(198, 18, 220, 254),
    Ipv4Address::new(198, 18, 220, 1)
)]
#[case::unmatched_gateway(
    Ipv4Address::new(203, 0, 113, 7),
    None,
    Ipv4Address::new(198, 18, 222, 254),
    Ipv4Address::new(192, 0, 2, 1)
)]
#[case::mismatched_authorization(
    Ipv4Address::new(203, 0, 113, 7),
    Some(Ipv4Address::new(198, 18, 215, 1)),
    Ipv4Address::new(198, 18, 220, 254),
    Ipv4Address::new(198, 18, 220, 1)
)]
#[case::unspecified_source(
    Ipv4Address::UNSPECIFIED,
    Some(Ipv4Address::UNSPECIFIED),
    Ipv4Address::new(198, 18, 220, 254),
    Ipv4Address::new(198, 18, 220, 1)
)]
fn routed_ipv4_arp_announces_authorized_source_or_next_hop_prefix(
    #[case] source: Ipv4Address,
    #[case] authorized: Option<Ipv4Address>,
    #[case] next_hop: Ipv4Address,
    #[case] expected: Ipv4Address,
) {
    let (mut iface, _, _) = setup(Medium::Ethernet);
    iface.update_ip_addrs(|addresses| {
        addresses.clear();
        addresses
            .push(IpCidr::new(Ipv4Address::new(192, 0, 2, 1).into(), 24))
            .unwrap();
        addresses
            .push(IpCidr::new(Ipv4Address::new(198, 18, 220, 1).into(), 24))
            .unwrap();
    });
    // AnyIP is enabled in DragonOS for routed weak-host input. It must not
    // cause a foreign forwarded source to become an owned ARP announcement.
    iface.set_any_ip(true);
    let mut packet = serialized_ipv4_packet(Ipv4Address::new(198, 18, 221, 7));
    let mut header = Ipv4Packet::new_unchecked(&mut packet);
    header.set_src_addr(source);
    header.fill_checksum();
    let frames = std::rc::Rc::new(core::cell::RefCell::new(Vec::new()));
    assert_eq!(
        iface.dispatch_ip_packet_with_arp_source(
            Instant::from_millis(10),
            CapturingTxToken(frames.clone()),
            next_hop.into(),
            None,
            &packet,
            authorized.map(IpAddress::from),
        ),
        Err(IpPacketDispatchError::NeighborPending {
            retry_at: Instant::from_millis(1_010),
        })
    );
    // Source authorization must not bypass neighbor-discovery rate limiting.
    assert_eq!(
        iface.dispatch_ip_packet_with_arp_source(
            Instant::from_millis(11),
            CapturingTxToken(frames.clone()),
            next_hop.into(),
            None,
            &packet,
            authorized.map(IpAddress::from),
        ),
        Err(IpPacketDispatchError::NeighborPending {
            retry_at: Instant::from_millis(1_010),
        })
    );
    let frames = frames.borrow();
    assert_eq!(frames.len(), 1);
    let frame = EthernetFrame::new_checked(&frames[0]).unwrap();
    assert_eq!(frame.ethertype(), EthernetProtocol::Arp);
    assert_eq!(
        ArpRepr::parse(&ArpPacket::new_checked(frame.payload()).unwrap()).unwrap(),
        ArpRepr::EthernetIpv4 {
            operation: ArpOperation::Request,
            source_hardware_addr: iface.inner.hardware_addr.ethernet_or_panic(),
            source_protocol_addr: expected,
            target_hardware_addr: EthernetAddress::BROADCAST,
            target_protocol_addr: next_hop,
        }
    );
}

#[test]
#[cfg(all(feature = "medium-ethernet", feature = "socket-icmp"))]
fn ordinary_ipv4_arp_announces_secondary_source_before_neighbor_prefix() {
    let (mut iface, _, _) = setup(Medium::Ethernet);
    let secondary = Ipv4Address::new(198, 18, 220, 1);
    iface.update_ip_addrs(|addresses| {
        addresses.push(IpCidr::new(secondary.into(), 24)).unwrap();
    });
    let destination = Ipv4Address::new(192, 168, 1, 254);
    let packet = Packet::new_ipv4(
        Ipv4Repr {
            src_addr: secondary,
            dst_addr: destination,
            next_header: IpProtocol::Icmp,
            payload_len: 8,
            hop_limit: 64,
        },
        IpPayload::Icmpv4(Icmpv4Repr::EchoRequest {
            ident: 1,
            seq_no: 1,
            data: &[],
        }),
    );
    let frames = std::rc::Rc::new(core::cell::RefCell::new(Vec::new()));
    assert_eq!(
        iface.inner.dispatch_ip(
            CapturingTxToken(frames.clone()),
            PacketMeta::default(),
            packet,
            &mut iface.fragmenter,
        ),
        Err(DispatchError::NeighborPending)
    );
    let frames = frames.borrow();
    assert_eq!(frames.len(), 1);
    let frame = EthernetFrame::new_checked(&frames[0]).unwrap();
    let arp = ArpPacket::new_checked(frame.payload()).unwrap();
    assert_eq!(arp.source_protocol_addr(), &secondary.octets()[..]);
    assert_eq!(arp.target_protocol_addr(), &destination.octets()[..]);
}

#[test]
#[cfg(feature = "medium-ethernet")]
fn explicit_ipv4_dispatch_uses_supplied_next_hop_neighbor() {
    let (mut iface, _, _) = setup(Medium::Ethernet);
    iface.set_neighbor_discovery_enabled(false);
    let frames = std::rc::Rc::new(core::cell::RefCell::new(Vec::new()));
    let destination_mac = EthernetAddress::from_bytes(&[0x02, 0, 0, 0, 0, 9]);
    let packet = serialized_ipv4_packet(Ipv4Address::new(198, 51, 100, 7));

    assert_eq!(
        iface.dispatch_ipv4_packet(
            Instant::from_millis(10),
            CapturingTxToken(frames.clone()),
            Ipv4Address::new(192, 0, 2, 1),
            Some(HardwareAddress::Ethernet(destination_mac)),
            &packet,
        ),
        Ok(())
    );

    let frames = frames.borrow();
    assert_eq!(frames.len(), 1);
    let frame = EthernetFrame::new_checked(&frames[0]).unwrap();
    assert_eq!(frame.dst_addr(), destination_mac);
    assert_eq!(frame.ethertype(), EthernetProtocol::Ipv4);
    assert_eq!(frame.payload(), packet);
}

#[test]
#[cfg(feature = "medium-ethernet")]
fn explicit_ipv4_dispatch_emits_rate_limited_arp_for_missing_neighbor() {
    let (mut iface, _, _) = setup(Medium::Ethernet);
    let frames = std::rc::Rc::new(core::cell::RefCell::new(Vec::new()));
    let next_hop = Ipv4Address::new(192, 168, 1, 99);
    let packet = serialized_ipv4_packet(Ipv4Address::new(198, 51, 100, 7));

    assert_eq!(
        iface.dispatch_ipv4_packet(
            Instant::from_millis(10),
            CapturingTxToken(frames.clone()),
            next_hop,
            None,
            &packet,
        ),
        Err(Ipv4PacketDispatchError::NeighborPending {
            retry_at: Instant::from_millis(1_010),
        })
    );
    {
        let frames = frames.borrow();
        assert_eq!(frames.len(), 1);
        let frame = EthernetFrame::new_checked(&frames[0]).unwrap();
        assert_eq!(frame.ethertype(), EthernetProtocol::Arp);
        let arp = ArpPacket::new_checked(frame.payload()).unwrap();
        assert_eq!(arp.target_protocol_addr(), next_hop.octets());
    }

    let suppressed = std::rc::Rc::new(core::cell::RefCell::new(Vec::new()));
    assert_eq!(
        iface.dispatch_ipv4_packet(
            Instant::from_millis(10),
            CapturingTxToken(suppressed.clone()),
            next_hop,
            None,
            &packet,
        ),
        Err(Ipv4PacketDispatchError::NeighborPending {
            retry_at: Instant::from_millis(1_010),
        })
    );
    assert!(suppressed.borrow().is_empty());

    assert!(!iface.is_neighbor_resolved(Instant::from_millis(20), IpAddress::Ipv4(next_hop)));
    iface.inner.neighbor_cache.fill(
        IpAddress::Ipv4(next_hop),
        HardwareAddress::Ethernet(EthernetAddress::from_bytes(&[0x02, 0, 0, 0, 0, 99])),
        Instant::from_millis(20),
    );
    assert!(iface.is_neighbor_resolved(Instant::from_millis(20), IpAddress::Ipv4(next_hop)));
}

#[test]
#[cfg(feature = "medium-ethernet")]
fn disabled_neighbor_discovery_flushes_cache_and_uses_direct_hardware_address() {
    let (mut iface, _, _) = setup(Medium::Ethernet);
    let frames = std::rc::Rc::new(core::cell::RefCell::new(Vec::new()));
    let next_hop = Ipv4Address::new(192, 168, 1, 99);
    let learned_mac = EthernetAddress::from_bytes(&[0x02, 0, 0, 0, 0, 99]);
    let local_mac = iface.hardware_addr().ethernet_or_panic();
    let packet = serialized_ipv4_packet(Ipv4Address::new(198, 51, 100, 7));
    iface.inner.neighbor_cache.fill(
        IpAddress::Ipv4(next_hop),
        HardwareAddress::Ethernet(learned_mac),
        Instant::from_millis(1),
    );

    iface.set_neighbor_discovery_enabled(false);
    assert!(!iface.neighbor_discovery_enabled());
    assert!(iface.inner.has_neighbor(&IpAddress::Ipv4(next_hop)));
    assert!(iface.is_neighbor_resolved(Instant::from_millis(10), IpAddress::Ipv4(next_hop)));
    assert_eq!(
        iface.dispatch_ipv4_packet(
            Instant::from_millis(10),
            CapturingTxToken(frames.clone()),
            next_hop,
            None,
            &packet,
        ),
        Ok(())
    );

    let frames = frames.borrow();
    assert_eq!(frames.len(), 1);
    let frame = EthernetFrame::new_checked(&frames[0]).unwrap();
    assert_eq!(frame.ethertype(), EthernetProtocol::Ipv4);
    assert_eq!(frame.dst_addr(), local_mac);
    drop(frames);

    iface.set_neighbor_discovery_enabled(true);
    assert_eq!(
        iface.inner.lookup_hardware_addr(
            MockTxToken,
            &IpAddress::Ipv4(next_hop),
            None,
            &mut iface.fragmenter,
        ),
        Err(DispatchError::NeighborPending)
    );
}

#[test]
#[cfg(feature = "medium-ethernet")]
fn flushing_neighbor_cache_clears_mappings_and_rate_limits_without_changing_policy() {
    let (mut iface, _, _) = setup(Medium::Ethernet);
    let learned_addr = IpAddress::Ipv4(Ipv4Address::new(192, 0, 2, 10));
    let rate_limited_addr = IpAddress::Ipv4(Ipv4Address::new(192, 0, 2, 11));
    iface.inner.neighbor_cache.fill(
        learned_addr,
        HardwareAddress::Ethernet(EthernetAddress::from_bytes(&[0x02, 0, 0, 0, 0, 10])),
        Instant::ZERO,
    );
    iface
        .inner
        .neighbor_cache
        .limit_rate(rate_limited_addr, Instant::ZERO);

    iface.flush_neighbor_cache();

    assert_eq!(
        iface
            .inner
            .neighbor_cache
            .lookup(&learned_addr, Instant::ZERO),
        NeighborAnswer::NotFound
    );
    assert_eq!(
        iface
            .inner
            .neighbor_cache
            .lookup(&rate_limited_addr, Instant::ZERO),
        NeighborAnswer::NotFound
    );
    assert!(iface.neighbor_discovery_enabled());

    // An empty cache is a valid state and flushing it again is harmless.
    iface.flush_neighbor_cache();
}

#[test]
#[cfg(all(feature = "medium-ethernet", feature = "proto-ipv4-fragmentation"))]
fn flushing_neighbor_cache_drops_fragment_with_stale_link_layer_destination() {
    let (mut iface, _, _) = setup(Medium::Ethernet);
    iface.fragmenter.packet_len = 100;
    iface.fragmenter.sent_bytes = 40;
    iface.fragmenter.ipv4.dst_hardware_addr = EthernetAddress::from_bytes(&[0x02, 0, 0, 0, 0, 99]);

    iface.flush_neighbor_cache();

    assert!(iface.fragmenter.is_empty());
    assert_eq!(
        iface.fragmenter.ipv4.dst_hardware_addr,
        EthernetAddress::default()
    );
    assert!(iface.neighbor_discovery_enabled());
}

#[test]
#[cfg(all(feature = "medium-ethernet", feature = "proto-ipv4-fragmentation"))]
fn disabling_neighbor_discovery_drops_pending_fragment_with_cached_hardware_address() {
    let (mut iface, _, _) = setup(Medium::Ethernet);
    iface.fragmenter.packet_len = 100;
    iface.fragmenter.sent_bytes = 40;
    iface.fragmenter.ipv4.dst_hardware_addr = EthernetAddress::from_bytes(&[0x02, 0, 0, 0, 0, 99]);

    iface.set_neighbor_discovery_enabled(false);

    assert!(iface.fragmenter.is_empty());
    assert_eq!(
        iface.fragmenter.ipv4.dst_hardware_addr,
        EthernetAddress::default()
    );
}

#[test]
#[cfg(all(feature = "medium-ethernet", feature = "proto-ipv4-fragmentation"))]
fn enabling_neighbor_discovery_drops_pending_direct_fragment() {
    let (mut iface, _, _) = setup(Medium::Ethernet);
    iface.set_neighbor_discovery_enabled(false);
    iface.fragmenter.packet_len = 100;
    iface.fragmenter.sent_bytes = 40;
    iface.fragmenter.ipv4.dst_hardware_addr = iface.hardware_addr().ethernet_or_panic();

    iface.set_neighbor_discovery_enabled(true);

    assert!(iface.fragmenter.is_empty());
    assert_eq!(
        iface.fragmenter.ipv4.dst_hardware_addr,
        EthernetAddress::default()
    );
}

#[test]
#[cfg(feature = "medium-ip")]
fn explicit_ipv4_dispatch_preserves_packet_on_ip_medium() {
    let (mut iface, _, _) = setup(Medium::Ip);
    let frames = std::rc::Rc::new(core::cell::RefCell::new(Vec::new()));
    let packet = serialized_ipv4_packet(Ipv4Address::new(127, 0, 0, 1));
    assert_eq!(
        iface.dispatch_ipv4_packet(
            Instant::from_millis(10),
            CapturingTxToken(frames.clone()),
            Ipv4Address::new(127, 0, 0, 1),
            None,
            &packet,
        ),
        Ok(())
    );
    assert_eq!(frames.borrow().as_slice(), &[packet]);
}

#[test]
#[cfg(all(feature = "proto-ipv4", feature = "medium-ip", feature = "socket-raw"))]
fn egress_admission_failure_does_not_consume_token() {
    struct ExhaustedTxToken;

    impl TxToken for ExhaustedTxToken {
        fn egress_override(
            &mut self,
            _version: IpVersion,
            _destination: IpAddress,
            _meta: PacketMeta,
        ) -> Result<Option<crate::phy::TxEgressOverride>, crate::phy::TxEgressError> {
            Err(crate::phy::TxEgressError::Exhausted)
        }

        fn consume<R, F>(self, _len: usize, _f: F) -> R
        where
            F: FnOnce(&mut [u8]) -> R,
        {
            panic!("an exhausted backend must not consume its token")
        }
    }

    let (mut iface, _, _) = setup(Medium::Ip);
    let payload = [1, 2, 3, 4];
    let packet = Packet::new_ipv4(
        Ipv4Repr {
            src_addr: Ipv4Address::new(192, 0, 2, 1),
            dst_addr: Ipv4Address::new(198, 51, 100, 7),
            next_header: IpProtocol::Udp,
            payload_len: payload.len(),
            hop_limit: 64,
        },
        IpPayload::Raw(&payload),
    );

    assert_eq!(
        iface.inner.dispatch_ip(
            ExhaustedTxToken,
            PacketMeta::default(),
            packet,
            &mut iface.fragmenter,
        ),
        Err(DispatchError::Exhausted)
    );
}

#[rstest]
#[case(Medium::Ip)]
#[cfg(feature = "medium-ip")]
#[case(Medium::Ethernet)]
#[cfg(feature = "medium-ethernet")]
fn test_route_uses_explicit_longest_prefix_and_preserves_broadcast(#[case] medium: Medium) {
    let (mut iface, _, _) = setup(medium);
    iface.set_route_table_includes_connected_prefixes(true);
    let gateway = IpAddress::v4(192, 168, 1, 2);

    iface.routes_mut().update(|routes| {
        let direct = crate::iface::Route {
            cidr: IpCidr::new(IpAddress::v4(192, 168, 1, 0), 24),
            via_router: None,
            preferred_until: None,
            expires_at: None,
        };
        let more_specific = crate::iface::Route {
            cidr: IpCidr::new(IpAddress::v4(192, 168, 1, 128), 25),
            via_router: Some(gateway),
            preferred_until: None,
            expires_at: None,
        };
        #[cfg(feature = "alloc")]
        {
            routes.push(direct);
            routes.push(more_specific);
        }
        #[cfg(not(feature = "alloc"))]
        {
            routes.push(direct).unwrap();
            routes.push(more_specific).unwrap();
        }
    });

    assert_eq!(
        iface
            .inner
            .route(&IpAddress::v4(192, 168, 1, 42), Instant::ZERO),
        Some(IpAddress::v4(192, 168, 1, 42))
    );
    assert_eq!(
        iface
            .inner
            .route(&IpAddress::v4(192, 168, 1, 200), Instant::ZERO),
        Some(gateway)
    );
    assert_eq!(
        iface
            .inner
            .route(&IpAddress::v4(192, 168, 1, 255), Instant::ZERO),
        Some(IpAddress::v4(192, 168, 1, 255))
    );

    iface.routes_mut().update(|routes| routes.clear());
    assert_eq!(
        iface
            .inner
            .route(&IpAddress::v4(192, 168, 1, 42), Instant::ZERO),
        None
    );
}

#[rstest]
#[case(Medium::Ethernet)]
#[cfg(feature = "medium-ethernet")]
fn test_any_ip_accept_arp(#[case] medium: Medium) {
    let mut buffer = [0u8; 64];
    #[allow(non_snake_case)]
    fn ETHERNET_FRAME_ARP(buffer: &mut [u8]) -> &[u8] {
        let ethernet_repr = EthernetRepr {
            src_addr: EthernetAddress::from_bytes(&[0x02, 0x02, 0x02, 0x02, 0x02, 0x03]),
            dst_addr: EthernetAddress::from_bytes(&[0x02, 0x02, 0x02, 0x02, 0x02, 0x02]),
            ethertype: EthernetProtocol::Arp,
        };
        let frame_repr = ArpRepr::EthernetIpv4 {
            operation: ArpOperation::Request,
            source_hardware_addr: EthernetAddress::from_bytes(&[
                0x02, 0x02, 0x02, 0x02, 0x02, 0x03,
            ]),
            source_protocol_addr: Ipv4Address::from_bytes(&[192, 168, 1, 2]),
            target_hardware_addr: EthernetAddress::from_bytes(&[
                0x02, 0x02, 0x02, 0x02, 0x02, 0x02,
            ]),
            target_protocol_addr: Ipv4Address::from_bytes(&[192, 168, 1, 3]),
        };
        let mut frame = EthernetFrame::new_unchecked(&mut buffer[..]);
        ethernet_repr.emit(&mut frame);

        let mut frame = ArpPacket::new_unchecked(&mut buffer[ethernet_repr.buffer_len()..]);
        frame_repr.emit(&mut frame);

        &buffer[..ethernet_repr.buffer_len() + frame_repr.buffer_len()]
    }

    let (mut iface, mut sockets, _) = setup(medium);

    assert!(iface
        .inner
        .process_ethernet(
            &mut sockets,
            PacketMeta::default(),
            ETHERNET_FRAME_ARP(buffer.as_mut()),
            &mut iface.fragments,
        )
        .is_none());

    // Accept any IP address
    iface.set_any_ip(true);

    assert!(iface
        .inner
        .process_ethernet(
            &mut sockets,
            PacketMeta::default(),
            ETHERNET_FRAME_ARP(buffer.as_mut()),
            &mut iface.fragments,
        )
        .is_some());
}

#[rstest]
#[case(Medium::Ip)]
#[cfg(feature = "medium-ip")]
#[case(Medium::Ethernet)]
#[cfg(feature = "medium-ethernet")]
fn test_no_icmp_no_unicast(#[case] medium: Medium) {
    let (mut iface, mut sockets, _) = setup(medium);

    // Unknown Ipv4 Protocol
    //
    // Because the destination is the broadcast address
    // this should not trigger and Destination Unreachable
    // response. See RFC 1122 § 3.2.2.
    let repr = IpRepr::Ipv4(Ipv4Repr {
        src_addr: Ipv4Address::new(0x7f, 0x00, 0x00, 0x01),
        dst_addr: Ipv4Address::BROADCAST,
        next_header: IpProtocol::Unknown(0x0c),
        payload_len: 0,
        hop_limit: 0x40,
    });

    let mut bytes = vec![0u8; 54];
    repr.emit(&mut bytes, &ChecksumCapabilities::default());
    let frame = Ipv4Packet::new_unchecked(&bytes[..]);

    // Ensure that the unknown protocol frame does not trigger an
    // ICMP error response when the destination address is a
    // broadcast address

    assert_eq!(
        iface.inner.process_ipv4(
            &mut sockets,
            PacketMeta::default(),
            HardwareAddress::default(),
            &frame,
            &mut iface.fragments
        ),
        None
    );
}

#[cfg(all(feature = "alloc", feature = "medium-ip", feature = "socket-tcp"))]
#[test]
fn explicit_broadcast_route_suppresses_tcp_reset_and_protocol_error() {
    struct BroadcastRoute;

    impl IpIngressFilter for BroadcastRoute {
        fn pre_routing(
            &mut self,
            _: &mut IngressPacket<'_>,
            _: PacketMeta,
            _: HardwareAddress,
        ) -> PreRoutingVerdict {
            PreRoutingVerdict::Pass
        }

        fn broadcast_route_selected(&self) -> bool {
            true
        }
    }

    let (mut iface, mut sockets, _) = setup(Medium::Ip);
    let src = Ipv4Address::new(127, 0, 0, 2);
    let dst = Ipv4Address::new(127, 0, 0, 1);
    let tcp = TcpRepr {
        src_port: 4242,
        dst_port: 4243,
        control: TcpControl::Syn,
        seq_number: TcpSeqNumber(1),
        ack_number: None,
        urgent_at: None,
        window_len: 256,
        window_scale: None,
        max_seg_size: None,
        sack_permitted: false,
        sack_ranges: [None, None, None],
        timestamp: None,
        payload: &[],
    };
    let mut tcp_payload = vec![0; tcp.buffer_len()];
    tcp.emit(
        &mut TcpPacket::new_unchecked(&mut tcp_payload),
        &src.into(),
        &dst.into(),
        &ChecksumCapabilities::default(),
    );

    for (protocol, payload) in [
        (IpProtocol::Tcp, tcp_payload.as_slice()),
        (IpProtocol::Unknown(253), &[][..]),
    ] {
        let repr = Ipv4Repr {
            src_addr: src,
            dst_addr: dst,
            next_header: protocol,
            payload_len: payload.len(),
            hop_limit: 64,
        };
        let mut bytes = vec![0; repr.buffer_len() + payload.len()];
        repr.emit(
            &mut Ipv4Packet::new_unchecked(&mut bytes),
            &ChecksumCapabilities::default(),
        );
        bytes[repr.buffer_len()..].copy_from_slice(payload);
        let packet = Ipv4Packet::new_checked(&bytes[..]).unwrap();
        let mut scratch = crate::storage::PacketBytes::new();
        assert!(iface
            .inner
            .process_ipv4_filtered(
                &mut sockets,
                PacketMeta::default(),
                HardwareAddress::Ip,
                &packet,
                &mut iface.fragments,
                &mut scratch,
                &mut BroadcastRoute,
            )
            .is_none());
    }
}

#[rstest]
#[case(Medium::Ip)]
#[cfg(feature = "medium-ip")]
#[case(Medium::Ethernet)]
#[cfg(feature = "medium-ethernet")]
fn test_icmp_error_no_payload(#[case] medium: Medium) {
    static NO_BYTES: [u8; 0] = [];
    let (mut iface, mut sockets, _device) = setup(medium);

    // Unknown Ipv4 Protocol with no payload
    let repr = IpRepr::Ipv4(Ipv4Repr {
        src_addr: Ipv4Address::new(0x7f, 0x00, 0x00, 0x02),
        dst_addr: Ipv4Address::new(0x7f, 0x00, 0x00, 0x01),
        next_header: IpProtocol::Unknown(0x0c),
        payload_len: 0,
        hop_limit: 0x40,
    });

    let mut bytes = vec![0u8; 34];
    repr.emit(&mut bytes, &ChecksumCapabilities::default());
    let frame = Ipv4Packet::new_unchecked(&bytes[..]);

    // The expected Destination Unreachable response due to the
    // unknown protocol
    let icmp_repr = Icmpv4Repr::DstUnreachable {
        reason: Icmpv4DstUnreachable::ProtoUnreachable,
        header: Ipv4Repr {
            src_addr: Ipv4Address::new(0x7f, 0x00, 0x00, 0x02),
            dst_addr: Ipv4Address::new(0x7f, 0x00, 0x00, 0x01),
            next_header: IpProtocol::Unknown(12),
            payload_len: 0,
            hop_limit: 64,
        },
        data: &NO_BYTES,
    };

    let expected_repr = Packet::new_ipv4(
        Ipv4Repr {
            src_addr: Ipv4Address::new(0x7f, 0x00, 0x00, 0x01),
            dst_addr: Ipv4Address::new(0x7f, 0x00, 0x00, 0x02),
            next_header: IpProtocol::Icmp,
            payload_len: icmp_repr.buffer_len(),
            hop_limit: 64,
        },
        IpPayload::Icmpv4(icmp_repr),
    );

    // Ensure that the unknown protocol triggers an error response.
    // And we correctly handle no payload.

    assert_eq!(
        iface.inner.process_ipv4(
            &mut sockets,
            PacketMeta::default(),
            HardwareAddress::default(),
            &frame,
            &mut iface.fragments
        ),
        Some(expected_repr)
    );
}

#[rstest]
#[case(Medium::Ip)]
#[cfg(feature = "medium-ip")]
#[case(Medium::Ethernet)]
#[cfg(feature = "medium-ethernet")]
fn test_local_subnet_broadcasts(#[case] medium: Medium) {
    let (mut iface, _, _device) = setup(medium);
    iface.update_ip_addrs(|addrs| {
        addrs.iter_mut().next().map(|addr| {
            *addr = IpCidr::Ipv4(Ipv4Cidr::new(Ipv4Address::new(192, 168, 1, 23), 24));
        });
    });

    assert!(iface
        .inner
        .is_broadcast_v4(Ipv4Address::new(255, 255, 255, 255)));
    assert!(!iface
        .inner
        .is_broadcast_v4(Ipv4Address::new(255, 255, 255, 254)));
    assert!(iface
        .inner
        .is_broadcast_v4(Ipv4Address::new(192, 168, 1, 255)));
    assert!(!iface
        .inner
        .is_broadcast_v4(Ipv4Address::new(192, 168, 1, 254)));

    iface.update_ip_addrs(|addrs| {
        addrs.iter_mut().next().map(|addr| {
            *addr = IpCidr::Ipv4(Ipv4Cidr::new(Ipv4Address::new(192, 168, 23, 24), 16));
        });
    });
    assert!(iface
        .inner
        .is_broadcast_v4(Ipv4Address::new(255, 255, 255, 255)));
    assert!(!iface
        .inner
        .is_broadcast_v4(Ipv4Address::new(255, 255, 255, 254)));
    assert!(!iface
        .inner
        .is_broadcast_v4(Ipv4Address::new(192, 168, 23, 255)));
    assert!(!iface
        .inner
        .is_broadcast_v4(Ipv4Address::new(192, 168, 23, 254)));
    assert!(!iface
        .inner
        .is_broadcast_v4(Ipv4Address::new(192, 168, 255, 254)));
    assert!(iface
        .inner
        .is_broadcast_v4(Ipv4Address::new(192, 168, 255, 255)));

    iface.update_ip_addrs(|addrs| {
        addrs.iter_mut().next().map(|addr| {
            *addr = IpCidr::Ipv4(Ipv4Cidr::new(Ipv4Address::new(192, 168, 23, 24), 8));
        });
    });
    assert!(iface
        .inner
        .is_broadcast_v4(Ipv4Address::new(255, 255, 255, 255)));
    assert!(!iface
        .inner
        .is_broadcast_v4(Ipv4Address::new(255, 255, 255, 254)));
    assert!(!iface
        .inner
        .is_broadcast_v4(Ipv4Address::new(192, 23, 1, 255)));
    assert!(!iface
        .inner
        .is_broadcast_v4(Ipv4Address::new(192, 23, 1, 254)));
    assert!(!iface
        .inner
        .is_broadcast_v4(Ipv4Address::new(192, 255, 255, 254)));
    assert!(iface
        .inner
        .is_broadcast_v4(Ipv4Address::new(192, 255, 255, 255)));
}

#[rstest]
#[case(Medium::Ip)]
#[cfg(all(feature = "medium-ip", feature = "socket-udp"))]
#[case(Medium::Ethernet)]
#[cfg(all(feature = "medium-ethernet", feature = "socket-udp"))]
fn test_icmp_error_port_unreachable(#[case] medium: Medium) {
    static UDP_PAYLOAD: [u8; 12] = [
        0x48, 0x65, 0x6c, 0x6c, 0x6f, 0x2c, 0x20, 0x57, 0x6f, 0x6c, 0x64, 0x21,
    ];
    let (mut iface, mut sockets, _device) = setup(medium);

    let mut udp_bytes_unicast = vec![0u8; 20];
    let mut udp_bytes_broadcast = vec![0u8; 20];
    let mut packet_unicast = UdpPacket::new_unchecked(&mut udp_bytes_unicast);
    let mut packet_broadcast = UdpPacket::new_unchecked(&mut udp_bytes_broadcast);

    let udp_repr = UdpRepr {
        src_port: 67,
        dst_port: 68,
    };

    let ip_repr = IpRepr::Ipv4(Ipv4Repr {
        src_addr: Ipv4Address::new(0x7f, 0x00, 0x00, 0x02),
        dst_addr: Ipv4Address::new(0x7f, 0x00, 0x00, 0x01),
        next_header: IpProtocol::Udp,
        payload_len: udp_repr.header_len() + UDP_PAYLOAD.len(),
        hop_limit: 64,
    });

    // Emit the representations to a packet
    udp_repr.emit(
        &mut packet_unicast,
        &ip_repr.src_addr(),
        &ip_repr.dst_addr(),
        UDP_PAYLOAD.len(),
        |buf| buf.copy_from_slice(&UDP_PAYLOAD),
        &ChecksumCapabilities::default(),
    );

    let data = packet_unicast.into_inner();

    // The expected Destination Unreachable ICMPv4 error response due
    // to no sockets listening on the destination port.
    let icmp_repr = Icmpv4Repr::DstUnreachable {
        reason: Icmpv4DstUnreachable::PortUnreachable,
        header: Ipv4Repr {
            src_addr: Ipv4Address::new(0x7f, 0x00, 0x00, 0x02),
            dst_addr: Ipv4Address::new(0x7f, 0x00, 0x00, 0x01),
            next_header: IpProtocol::Udp,
            payload_len: udp_repr.header_len() + UDP_PAYLOAD.len(),
            hop_limit: 64,
        },
        data,
    };
    let expected_repr = Packet::new_ipv4(
        Ipv4Repr {
            src_addr: Ipv4Address::new(0x7f, 0x00, 0x00, 0x01),
            dst_addr: Ipv4Address::new(0x7f, 0x00, 0x00, 0x02),
            next_header: IpProtocol::Icmp,
            payload_len: icmp_repr.buffer_len(),
            hop_limit: 64,
        },
        IpPayload::Icmpv4(icmp_repr),
    );

    // Ensure that the unknown protocol triggers an error response.
    // And we correctly handle no payload.
    assert_eq!(
        iface
            .inner
            .process_udp(&mut sockets, PacketMeta::default(), ip_repr, data, false),
        Some(expected_repr)
    );

    let ip_repr = IpRepr::Ipv4(Ipv4Repr {
        src_addr: Ipv4Address::new(0x7f, 0x00, 0x00, 0x02),
        dst_addr: Ipv4Address::BROADCAST,
        next_header: IpProtocol::Udp,
        payload_len: udp_repr.header_len() + UDP_PAYLOAD.len(),
        hop_limit: 64,
    });

    // Emit the representations to a packet
    udp_repr.emit(
        &mut packet_broadcast,
        &ip_repr.src_addr(),
        &IpAddress::Ipv4(Ipv4Address::BROADCAST),
        UDP_PAYLOAD.len(),
        |buf| buf.copy_from_slice(&UDP_PAYLOAD),
        &ChecksumCapabilities::default(),
    );

    // Ensure that the port unreachable error does not trigger an
    // ICMP error response when the destination address is a
    // broadcast address and no socket is bound to the port.
    assert_eq!(
        iface.inner.process_udp(
            &mut sockets,
            PacketMeta::default(),
            ip_repr,
            packet_broadcast.into_inner(),
            false,
        ),
        None
    );
}

#[rstest]
#[case(Medium::Ip)]
#[cfg(feature = "medium-ip")]
#[case(Medium::Ethernet)]
#[cfg(feature = "medium-ethernet")]
fn test_handle_ipv4_broadcast(#[case] medium: Medium) {
    use crate::wire::{Icmpv4Packet, Icmpv4Repr};

    let (mut iface, mut sockets, _device) = setup(medium);

    let our_ipv4_addr = iface.ipv4_addr().unwrap();
    let src_ipv4_addr = Ipv4Address::new(127, 0, 0, 2);

    // ICMPv4 echo request
    let icmpv4_data: [u8; 4] = [0xaa, 0x00, 0x00, 0xff];
    let icmpv4_repr = Icmpv4Repr::EchoRequest {
        ident: 0x1234,
        seq_no: 0xabcd,
        data: &icmpv4_data,
    };

    // Send to IPv4 broadcast address
    let ipv4_repr = Ipv4Repr {
        src_addr: src_ipv4_addr,
        dst_addr: Ipv4Address::BROADCAST,
        next_header: IpProtocol::Icmp,
        hop_limit: 64,
        payload_len: icmpv4_repr.buffer_len(),
    };

    // Emit to ip frame
    let mut bytes = vec![0u8; ipv4_repr.buffer_len() + icmpv4_repr.buffer_len()];
    let frame = {
        ipv4_repr.emit(
            &mut Ipv4Packet::new_unchecked(&mut bytes[..]),
            &ChecksumCapabilities::default(),
        );
        icmpv4_repr.emit(
            &mut Icmpv4Packet::new_unchecked(&mut bytes[ipv4_repr.buffer_len()..]),
            &ChecksumCapabilities::default(),
        );
        Ipv4Packet::new_unchecked(&bytes[..])
    };

    // Expected ICMPv4 echo reply
    let expected_icmpv4_repr = Icmpv4Repr::EchoReply {
        ident: 0x1234,
        seq_no: 0xabcd,
        data: &icmpv4_data,
    };
    let expected_ipv4_repr = Ipv4Repr {
        src_addr: our_ipv4_addr,
        dst_addr: src_ipv4_addr,
        next_header: IpProtocol::Icmp,
        hop_limit: 64,
        payload_len: expected_icmpv4_repr.buffer_len(),
    };
    let expected_packet =
        Packet::new_ipv4(expected_ipv4_repr, IpPayload::Icmpv4(expected_icmpv4_repr));

    assert_eq!(
        iface.inner.process_ipv4(
            &mut sockets,
            PacketMeta::default(),
            HardwareAddress::default(),
            &frame,
            &mut iface.fragments
        ),
        Some(expected_packet)
    );
}

#[rstest]
#[case(Medium::Ethernet)]
#[cfg(feature = "medium-ethernet")]
fn test_handle_valid_arp_request(#[case] medium: Medium) {
    let (mut iface, mut sockets, _device) = setup(medium);

    let mut eth_bytes = vec![0u8; 42];

    let local_ip_addr = Ipv4Address::new(0x7f, 0x00, 0x00, 0x01);
    let remote_ip_addr = Ipv4Address::new(0x7f, 0x00, 0x00, 0x02);
    let local_hw_addr = EthernetAddress([0x02, 0x02, 0x02, 0x02, 0x02, 0x02]);
    let remote_hw_addr = EthernetAddress([0x52, 0x54, 0x00, 0x00, 0x00, 0x00]);

    let repr = ArpRepr::EthernetIpv4 {
        operation: ArpOperation::Request,
        source_hardware_addr: remote_hw_addr,
        source_protocol_addr: remote_ip_addr,
        target_hardware_addr: EthernetAddress::default(),
        target_protocol_addr: local_ip_addr,
    };

    let mut frame = EthernetFrame::new_unchecked(&mut eth_bytes);
    frame.set_dst_addr(EthernetAddress::BROADCAST);
    frame.set_src_addr(remote_hw_addr);
    frame.set_ethertype(EthernetProtocol::Arp);
    let mut packet = ArpPacket::new_unchecked(frame.payload_mut());
    repr.emit(&mut packet);

    iface.set_neighbor_discovery_enabled(false);
    assert_eq!(
        iface.inner.process_ethernet(
            &mut sockets,
            PacketMeta::default(),
            frame.into_inner(),
            &mut iface.fragments
        ),
        None
    );
    iface.set_neighbor_discovery_enabled(true);
    assert_eq!(
        iface.inner.lookup_hardware_addr(
            MockTxToken,
            &IpAddress::Ipv4(remote_ip_addr),
            None,
            &mut iface.fragmenter,
        ),
        Err(DispatchError::NeighborPending)
    );

    let frame = EthernetFrame::new_unchecked(&mut eth_bytes);

    // Ensure an ARP Request for us triggers an ARP Reply
    assert_eq!(
        iface.inner.process_ethernet(
            &mut sockets,
            PacketMeta::default(),
            frame.into_inner(),
            &mut iface.fragments
        ),
        Some(EthernetPacket::Arp(ArpRepr::EthernetIpv4 {
            operation: ArpOperation::Reply,
            source_hardware_addr: local_hw_addr,
            source_protocol_addr: local_ip_addr,
            target_hardware_addr: remote_hw_addr,
            target_protocol_addr: remote_ip_addr
        }))
    );

    // Ensure the address of the requester was entered in the cache
    assert_eq!(
        iface.inner.lookup_hardware_addr(
            MockTxToken,
            &IpAddress::Ipv4(remote_ip_addr),
            None,
            &mut iface.fragmenter,
        ),
        Ok((HardwareAddress::Ethernet(remote_hw_addr), MockTxToken))
    );
}

#[test]
#[cfg(all(feature = "proto-ipv4-fragmentation", feature = "medium-ip"))]
fn pending_native_fragments_observe_runtime_mtu_change() {
    use core::cell::RefCell;
    use std::rc::Rc;

    #[derive(Clone)]
    struct BoundedTxToken {
        limit: usize,
        lengths: Rc<RefCell<Vec<usize>>>,
    }

    impl TxToken for BoundedTxToken {
        fn consume<R, F>(self, len: usize, f: F) -> R
        where
            F: FnOnce(&mut [u8]) -> R,
        {
            assert!(len <= self.limit);
            self.lengths.borrow_mut().push(len);
            let mut buffer = vec![0; len];
            f(&mut buffer)
        }
    }

    let (mut iface, _, _) = setup(Medium::Ip);
    iface.set_ip_mtu(1200).unwrap();
    let lengths = Rc::new(RefCell::new(Vec::new()));
    let payload = vec![0xa5; 1800];
    let packet = Packet::new_ipv4(
        Ipv4Repr {
            src_addr: Ipv4Address::new(192, 0, 2, 1),
            dst_addr: Ipv4Address::new(198, 51, 100, 1),
            next_header: IpProtocol::Udp,
            payload_len: payload.len(),
            hop_limit: 64,
        },
        IpPayload::Raw(&payload),
    );

    iface
        .inner
        .dispatch_ip(
            BoundedTxToken {
                limit: 1200,
                lengths: lengths.clone(),
            },
            PacketMeta::default(),
            packet,
            &mut iface.fragmenter,
        )
        .unwrap();
    assert!(!iface.fragmenter.finished());

    iface.set_ip_mtu(576).unwrap();
    iface.inner.dispatch_ipv4_frag(
        BoundedTxToken {
            limit: 576,
            lengths: lengths.clone(),
        },
        &mut iface.fragmenter,
    );

    let lengths = lengths.borrow();
    assert_eq!(lengths.len(), 2);
    assert!(lengths[0] <= 1200);
    assert!(lengths[1] <= 576);
}

#[rstest]
#[case(Medium::Ethernet)]
#[cfg(feature = "medium-ethernet")]
fn test_handle_other_arp_request(#[case] medium: Medium) {
    let (mut iface, mut sockets, _device) = setup(medium);

    let mut eth_bytes = vec![0u8; 42];

    let remote_ip_addr = Ipv4Address::new(0x7f, 0x00, 0x00, 0x02);
    let remote_hw_addr = EthernetAddress([0x52, 0x54, 0x00, 0x00, 0x00, 0x00]);

    let repr = ArpRepr::EthernetIpv4 {
        operation: ArpOperation::Request,
        source_hardware_addr: remote_hw_addr,
        source_protocol_addr: remote_ip_addr,
        target_hardware_addr: EthernetAddress::default(),
        target_protocol_addr: Ipv4Address::new(0x7f, 0x00, 0x00, 0x03),
    };

    let mut frame = EthernetFrame::new_unchecked(&mut eth_bytes);
    frame.set_dst_addr(EthernetAddress::BROADCAST);
    frame.set_src_addr(remote_hw_addr);
    frame.set_ethertype(EthernetProtocol::Arp);
    let mut packet = ArpPacket::new_unchecked(frame.payload_mut());
    repr.emit(&mut packet);

    // Ensure an ARP Request for someone else does not trigger an ARP Reply
    assert_eq!(
        iface.inner.process_ethernet(
            &mut sockets,
            PacketMeta::default(),
            frame.into_inner(),
            &mut iface.fragments
        ),
        None
    );

    // Ensure the address of the requester was NOT entered in the cache
    assert_eq!(
        iface.inner.lookup_hardware_addr(
            MockTxToken,
            &IpAddress::Ipv4(remote_ip_addr),
            None,
            &mut iface.fragmenter,
        ),
        Err(DispatchError::NeighborPending)
    );
}

#[rstest]
#[case(Medium::Ethernet)]
#[cfg(feature = "medium-ethernet")]
fn test_arp_flush_after_update_ip(#[case] medium: Medium) {
    let (mut iface, mut sockets, _device) = setup(medium);

    let mut eth_bytes = vec![0u8; 42];

    let local_ip_addr = Ipv4Address::new(0x7f, 0x00, 0x00, 0x01);
    let remote_ip_addr = Ipv4Address::new(0x7f, 0x00, 0x00, 0x02);
    let local_hw_addr = EthernetAddress([0x02, 0x02, 0x02, 0x02, 0x02, 0x02]);
    let remote_hw_addr = EthernetAddress([0x52, 0x54, 0x00, 0x00, 0x00, 0x00]);

    let repr = ArpRepr::EthernetIpv4 {
        operation: ArpOperation::Request,
        source_hardware_addr: remote_hw_addr,
        source_protocol_addr: remote_ip_addr,
        target_hardware_addr: EthernetAddress::default(),
        target_protocol_addr: Ipv4Address::new(0x7f, 0x00, 0x00, 0x01),
    };

    let mut frame = EthernetFrame::new_unchecked(&mut eth_bytes);
    frame.set_dst_addr(EthernetAddress::BROADCAST);
    frame.set_src_addr(remote_hw_addr);
    frame.set_ethertype(EthernetProtocol::Arp);
    {
        let mut packet = ArpPacket::new_unchecked(frame.payload_mut());
        repr.emit(&mut packet);
    }

    // Ensure an ARP Request for us triggers an ARP Reply
    assert_eq!(
        iface.inner.process_ethernet(
            &mut sockets,
            PacketMeta::default(),
            frame.into_inner(),
            &mut iface.fragments
        ),
        Some(EthernetPacket::Arp(ArpRepr::EthernetIpv4 {
            operation: ArpOperation::Reply,
            source_hardware_addr: local_hw_addr,
            source_protocol_addr: local_ip_addr,
            target_hardware_addr: remote_hw_addr,
            target_protocol_addr: remote_ip_addr
        }))
    );

    // Ensure the address of the requester was entered in the cache
    assert_eq!(
        iface.inner.lookup_hardware_addr(
            MockTxToken,
            &IpAddress::Ipv4(remote_ip_addr),
            None,
            &mut iface.fragmenter,
        ),
        Ok((HardwareAddress::Ethernet(remote_hw_addr), MockTxToken))
    );

    // Update IP addrs to trigger ARP cache flush
    let local_ip_addr_new = Ipv4Address::new(0x7f, 0x00, 0x00, 0x01);
    iface.update_ip_addrs(|addrs| {
        addrs.iter_mut().next().map(|addr| {
            *addr = IpCidr::Ipv4(Ipv4Cidr::new(local_ip_addr_new, 24));
        });
    });

    // ARP cache flush after address change
    assert!(!iface.inner.has_neighbor(&IpAddress::Ipv4(remote_ip_addr)));
}

#[rstest]
#[case(Medium::Ip)]
#[cfg(all(feature = "socket-icmp", feature = "medium-ip"))]
#[case(Medium::Ethernet)]
#[cfg(all(feature = "socket-icmp", feature = "medium-ethernet"))]
fn test_icmpv4_socket(#[case] medium: Medium) {
    use crate::wire::Icmpv4Packet;

    let (mut iface, mut sockets, _device) = setup(medium);

    let rx_buffer = icmp::PacketBuffer::new(vec![icmp::PacketMetadata::EMPTY], vec![0; 24]);
    let tx_buffer = icmp::PacketBuffer::new(vec![icmp::PacketMetadata::EMPTY], vec![0; 24]);

    let icmpv4_socket = icmp::Socket::new(rx_buffer, tx_buffer);

    let socket_handle = sockets.add(icmpv4_socket);

    let ident = 0x1234;
    let seq_no = 0x5432;
    let echo_data = &[0xff; 16];

    let socket = sockets.get_mut::<icmp::Socket>(socket_handle);
    // Bind to the ID 0x1234
    assert_eq!(socket.bind(icmp::Endpoint::Ident(ident)), Ok(()));

    // Ensure the ident we bound to and the ident of the packet are the same.
    let mut bytes = [0xff; 24];
    let mut packet = Icmpv4Packet::new_unchecked(&mut bytes[..]);
    let echo_repr = Icmpv4Repr::EchoRequest {
        ident,
        seq_no,
        data: echo_data,
    };
    echo_repr.emit(&mut packet, &ChecksumCapabilities::default());
    let icmp_data = &*packet.into_inner();

    let ipv4_repr = Ipv4Repr {
        src_addr: Ipv4Address::new(0x7f, 0x00, 0x00, 0x02),
        dst_addr: Ipv4Address::new(0x7f, 0x00, 0x00, 0x01),
        next_header: IpProtocol::Icmp,
        payload_len: 24,
        hop_limit: 64,
    };

    // Open a socket and ensure the packet is handled due to the listening
    // socket.
    assert!(!sockets.get_mut::<icmp::Socket>(socket_handle).can_recv());

    // Confirm we still get EchoReply from `smoltcp` even with the ICMP socket listening
    let echo_reply = Icmpv4Repr::EchoReply {
        ident,
        seq_no,
        data: echo_data,
    };
    let ipv4_reply = Ipv4Repr {
        src_addr: ipv4_repr.dst_addr,
        dst_addr: ipv4_repr.src_addr,
        ..ipv4_repr
    };
    assert_eq!(
        iface
            .inner
            .process_icmpv4(&mut sockets, ipv4_repr, icmp_data, false),
        Some(Packet::new_ipv4(ipv4_reply, IpPayload::Icmpv4(echo_reply)))
    );

    let socket = sockets.get_mut::<icmp::Socket>(socket_handle);
    assert!(socket.can_recv());
    assert_eq!(
        socket.recv(),
        Ok((
            icmp_data,
            IpAddress::Ipv4(Ipv4Address::new(0x7f, 0x00, 0x00, 0x02))
        ))
    );
}

#[rstest]
#[case(Medium::Ip)]
#[cfg(all(feature = "multicast", feature = "medium-ip"))]
#[case(Medium::Ethernet)]
#[cfg(all(feature = "multicast", feature = "medium-ethernet"))]
fn test_handle_igmp(#[case] medium: Medium) {
    fn recv_igmp(
        device: &mut crate::tests::TestingDevice,
        timestamp: Instant,
    ) -> Vec<(Ipv4Repr, IgmpRepr)> {
        let caps = device.capabilities();
        let checksum_caps = &caps.checksum;
        recv_all(device, timestamp)
            .iter()
            .filter_map(|frame| {
                let ipv4_packet = match caps.medium {
                    #[cfg(feature = "medium-ethernet")]
                    Medium::Ethernet => {
                        let eth_frame = EthernetFrame::new_checked(frame).ok()?;
                        Ipv4Packet::new_checked(eth_frame.payload()).ok()?
                    }
                    #[cfg(feature = "medium-ip")]
                    Medium::Ip => Ipv4Packet::new_checked(&frame[..]).ok()?,
                    #[cfg(feature = "medium-ieee802154")]
                    Medium::Ieee802154 => todo!(),
                };
                let ipv4_repr = Ipv4Repr::parse(&ipv4_packet, checksum_caps).ok()?;
                let ip_payload = ipv4_packet.payload();
                let igmp_packet = IgmpPacket::new_checked(ip_payload).ok()?;
                let igmp_repr = IgmpRepr::parse(&igmp_packet).ok()?;
                Some((ipv4_repr, igmp_repr))
            })
            .collect::<Vec<_>>()
    }

    let groups = [
        Ipv4Address::new(224, 0, 0, 22),
        Ipv4Address::new(224, 0, 0, 56),
    ];

    let (mut iface, mut sockets, mut device) = setup(medium);

    // Join multicast groups
    let timestamp = Instant::ZERO;
    for group in &groups {
        iface.join_multicast_group(*group).unwrap();
    }
    iface.poll(timestamp, &mut device, &mut sockets);

    let reports = recv_igmp(&mut device, timestamp);
    assert_eq!(reports.len(), 2);
    for (i, group_addr) in groups.iter().enumerate() {
        assert_eq!(reports[i].0.next_header, IpProtocol::Igmp);
        assert_eq!(reports[i].0.dst_addr, *group_addr);
        assert_eq!(
            reports[i].1,
            IgmpRepr::MembershipReport {
                group_addr: *group_addr,
                version: IgmpVersion::Version2,
            }
        );
    }

    // General query
    const GENERAL_QUERY_BYTES: &[u8] = &[
        0x46, 0xc0, 0x00, 0x24, 0xed, 0xb4, 0x00, 0x00, 0x01, 0x02, 0x47, 0x43, 0xac, 0x16, 0x63,
        0x04, 0xe0, 0x00, 0x00, 0x01, 0x94, 0x04, 0x00, 0x00, 0x11, 0x64, 0xec, 0x8f, 0x00, 0x00,
        0x00, 0x00, 0x02, 0x0c, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00,
    ];
    device.rx_queue.push_back(GENERAL_QUERY_BYTES.to_vec());

    // Trigger processing until all packets received through the
    // loopback have been processed, including responses to
    // GENERAL_QUERY_BYTES. Therefore `recv_all()` would return 0
    // pkts that could be checked.
    iface.socket_ingress(
        &mut device,
        &mut sockets,
        #[cfg(feature = "alloc")]
        None,
    );

    // Leave multicast groups
    let timestamp = Instant::ZERO;
    for group in &groups {
        iface.leave_multicast_group(*group).unwrap();
    }
    iface.poll(timestamp, &mut device, &mut sockets);

    let leaves = recv_igmp(&mut device, timestamp);
    assert_eq!(leaves.len(), 2);
    for (i, group_addr) in groups.iter().cloned().enumerate() {
        assert_eq!(leaves[i].0.next_header, IpProtocol::Igmp);
        assert_eq!(leaves[i].0.dst_addr, IPV4_MULTICAST_ALL_ROUTERS);
        assert_eq!(leaves[i].1, IgmpRepr::LeaveGroup { group_addr });
    }
}

#[rstest]
#[case(Medium::Ip)]
#[cfg(all(feature = "proto-ipv4-fragmentation", feature = "medium-ip"))]
#[case(Medium::Ethernet)]
#[cfg(all(feature = "proto-ipv4-fragmentation", feature = "medium-ethernet"))]
fn test_packet_len(#[case] medium: Medium) {
    use crate::config::FRAGMENTATION_BUFFER_SIZE;

    let (mut iface, _, _) = setup(medium);

    struct TestTxToken {
        max_transmission_unit: usize,
    }

    impl TxToken for TestTxToken {
        fn consume<R, F>(self, len: usize, f: F) -> R
        where
            F: FnOnce(&mut [u8]) -> R,
        {
            net_debug!("TxToken get len: {}", len);
            assert!(len <= self.max_transmission_unit);
            let mut junk = [0; 1536];
            f(&mut junk[..len])
        }
    }

    iface.inner.neighbor_cache.fill(
        IpAddress::Ipv4(Ipv4Address::new(127, 0, 0, 1)),
        HardwareAddress::Ethernet(EthernetAddress::from_bytes(&[
            0x02, 0x02, 0x02, 0x02, 0x02, 0x02,
        ])),
        Instant::ZERO,
    );

    for ip_packet_len in [
        100,
        iface.inner.ip_mtu(),
        iface.inner.ip_mtu() + 1,
        FRAGMENTATION_BUFFER_SIZE,
    ] {
        net_debug!("ip_packet_len: {}", ip_packet_len);

        let mut ip_repr = Ipv4Repr {
            src_addr: Ipv4Address::new(127, 0, 0, 1),
            dst_addr: Ipv4Address::new(127, 0, 0, 1),
            next_header: IpProtocol::Udp,
            payload_len: 0,
            hop_limit: 64,
        };
        let udp_repr = UdpRepr {
            src_port: 12345,
            dst_port: 54321,
        };

        let ip_packet_payload_len = ip_packet_len - ip_repr.buffer_len();
        let udp_packet_payload_len = ip_packet_payload_len - udp_repr.header_len();
        ip_repr.payload_len = ip_packet_payload_len;

        let udp_packet_payload = vec![1; udp_packet_payload_len];
        let ip_payload = IpPayload::Udp(udp_repr, &udp_packet_payload);
        let ip_packet = Packet::new_ipv4(ip_repr, ip_payload);

        assert_eq!(
            iface.inner.dispatch_ip(
                TestTxToken {
                    max_transmission_unit: iface.inner.caps.max_transmission_unit
                },
                PacketMeta::default(),
                ip_packet,
                &mut iface.fragmenter,
            ),
            Ok(())
        );
    }
}

#[test]
#[cfg(all(feature = "proto-ipv4-fragmentation", feature = "medium-ip"))]
fn native_continuation_fragment_rechecks_backend_admission() {
    use core::cell::Cell;
    use std::rc::Rc;

    struct NativeTxToken;

    impl TxToken for NativeTxToken {
        fn consume<R, F>(self, len: usize, f: F) -> R
        where
            F: FnOnce(&mut [u8]) -> R,
        {
            let mut buffer = vec![0; len];
            f(&mut buffer)
        }
    }

    struct ExhaustedContinuation {
        admitted: Rc<Cell<bool>>,
    }

    impl TxToken for ExhaustedContinuation {
        fn apply_egress_override(
            &mut self,
            egress: Option<crate::phy::TxEgressOverride>,
        ) -> Result<(), crate::phy::TxEgressError> {
            assert_eq!(egress, None);
            self.admitted.set(true);
            Err(crate::phy::TxEgressError::Exhausted)
        }

        fn consume<R, F>(self, _len: usize, _f: F) -> R
        where
            F: FnOnce(&mut [u8]) -> R,
        {
            panic!("an exhausted continuation must remain pending")
        }
    }

    let (mut iface, _, _) = setup(Medium::Ip);
    let payload = vec![0xa5; iface.inner.ip_mtu() + 256];
    let packet = Packet::new_ipv4(
        Ipv4Repr {
            src_addr: Ipv4Address::new(192, 0, 2, 1),
            dst_addr: Ipv4Address::new(198, 51, 100, 1),
            next_header: IpProtocol::Udp,
            payload_len: payload.len(),
            hop_limit: 64,
        },
        IpPayload::Raw(&payload),
    );

    iface
        .inner
        .dispatch_ip(
            NativeTxToken,
            PacketMeta::default(),
            packet,
            &mut iface.fragmenter,
        )
        .unwrap();
    let sent_bytes = iface.fragmenter.sent_bytes;
    assert!(sent_bytes > 0 && sent_bytes < iface.fragmenter.packet_len);

    let admitted = Rc::new(Cell::new(false));
    iface.inner.dispatch_ipv4_frag(
        ExhaustedContinuation {
            admitted: admitted.clone(),
        },
        &mut iface.fragmenter,
    );
    assert!(admitted.get());
    assert_eq!(iface.fragmenter.sent_bytes, sent_bytes);
}

#[test]
#[cfg(all(
    feature = "proto-ipv4-fragmentation",
    feature = "medium-ethernet",
    feature = "medium-ip"
))]
fn routed_egress_override_controls_fragment_mtu_and_metadata() {
    use core::cell::RefCell;
    use std::rc::Rc;

    const ROUTED_IP_MTU: usize = 577;
    const META_ID: u32 = 17;
    const ROUTE_CONTEXT: [u64; 3] = [0x1234_5678_9abc_def0, 0x1122, 0x3344];

    type FragmentObservation = (usize, PacketMeta, Option<[u64; 3]>, u16, bool, usize);

    #[derive(Clone)]
    struct RoutedTxToken {
        observed: Rc<RefCell<Vec<FragmentObservation>>>,
        meta: PacketMeta,
        context: Option<[u64; 3]>,
    }

    impl TxToken for RoutedTxToken {
        fn egress_override(
            &mut self,
            _version: IpVersion,
            _destination: IpAddress,
            _meta: PacketMeta,
        ) -> Result<Option<crate::phy::TxEgressOverride>, crate::phy::TxEgressError> {
            Ok(Some(crate::phy::TxEgressOverride {
                medium: Medium::Ip,
                ip_mtu: ROUTED_IP_MTU,
                context: ROUTE_CONTEXT,
            }))
        }

        fn apply_egress_override(
            &mut self,
            egress: Option<crate::phy::TxEgressOverride>,
        ) -> Result<(), crate::phy::TxEgressError> {
            self.context = egress.map(|egress| egress.context);
            Ok(())
        }

        fn consume<R, F>(self, len: usize, f: F) -> R
        where
            F: FnOnce(&mut [u8]) -> R,
        {
            assert!(len <= ROUTED_IP_MTU);
            let mut buffer = vec![0; len];
            let result = f(&mut buffer);
            let packet = Ipv4Packet::new_checked(&buffer).unwrap();
            self.observed.borrow_mut().push((
                len,
                self.meta,
                self.context,
                packet.frag_offset(),
                packet.more_frags(),
                packet.header_len() as usize,
            ));
            result
        }

        fn set_meta(&mut self, meta: PacketMeta) {
            self.meta = meta;
        }
    }

    let (mut iface, _, _) = setup(Medium::Ethernet);
    let observed = Rc::new(RefCell::new(Vec::new()));
    let meta = PacketMeta { id: META_ID };
    let ip_repr = Ipv4Repr {
        src_addr: Ipv4Address::new(192, 0, 2, 1),
        dst_addr: Ipv4Address::new(198, 51, 100, 1),
        next_header: IpProtocol::Udp,
        payload_len: 2000,
        hop_limit: 64,
    };
    let udp_repr = UdpRepr {
        src_port: 12345,
        dst_port: 54321,
    };
    let payload = vec![0xa5; ip_repr.payload_len - udp_repr.header_len()];
    let packet = Packet::new_ipv4(ip_repr, IpPayload::Udp(udp_repr, &payload));

    iface
        .inner
        .dispatch_ip(
            RoutedTxToken {
                observed: observed.clone(),
                meta: PacketMeta::default(),
                context: None,
            },
            meta,
            packet,
            &mut iface.fragmenter,
        )
        .unwrap();

    while iface.fragmenter.sent_bytes < iface.fragmenter.packet_len {
        iface.inner.dispatch_ipv4_frag(
            RoutedTxToken {
                observed: observed.clone(),
                meta: PacketMeta::default(),
                context: None,
            },
            &mut iface.fragmenter,
        );
    }

    let observed = observed.borrow();
    assert!(observed.len() > 1);
    assert!(observed
        .iter()
        .all(|(len, meta, _, _, _, _)| *len <= ROUTED_IP_MTU && meta.id == META_ID));
    assert_eq!(observed[0].2, None);
    assert!(observed[1..]
        .iter()
        .all(|(_, _, context, _, _, _)| *context == Some(ROUTE_CONTEXT)));
    let mut expected_offset = 0;
    for (len, _, _, offset, more_fragments, header_len) in observed.iter() {
        assert_eq!(*offset as usize, expected_offset);
        let payload_len = len - header_len;
        if *more_fragments {
            assert_eq!(payload_len % 8, 0);
        }
        expected_offset += payload_len;
    }
}

#[rstest]
#[case(Medium::Ip)]
#[cfg(all(feature = "socket-raw", feature = "medium-ip"))]
#[case(Medium::Ethernet)]
#[cfg(all(feature = "socket-raw", feature = "medium-ethernet"))]
fn test_raw_socket_does_not_suppress_udp_port_unreachable(#[case] medium: Medium) {
    use crate::wire::{IpVersion, UdpPacket, UdpRepr};

    let (mut iface, mut sockets, _) = setup(medium);

    let packets = 1;
    let rx_buffer =
        raw::PacketBuffer::new(vec![raw::PacketMetadata::EMPTY; packets], vec![0; 48 * 1]);
    let tx_buffer = raw::PacketBuffer::new(
        vec![raw::PacketMetadata::EMPTY; packets],
        vec![0; 48 * packets],
    );
    let raw_socket = raw::Socket::new(IpVersion::Ipv4, IpProtocol::Udp, rx_buffer, tx_buffer);
    let raw_socket_handle = sockets.add(raw_socket);

    let src_addr = Ipv4Address::new(127, 0, 0, 2);
    let dst_addr = Ipv4Address::new(127, 0, 0, 1);

    const PAYLOAD_LEN: usize = 10;

    let udp_repr = UdpRepr {
        src_port: 67,
        dst_port: 68,
    };
    let mut bytes = vec![0xff; udp_repr.header_len() + PAYLOAD_LEN];
    let mut packet = UdpPacket::new_unchecked(&mut bytes[..]);
    udp_repr.emit(
        &mut packet,
        &src_addr.into(),
        &dst_addr.into(),
        PAYLOAD_LEN,
        |buf| fill_slice(buf, 0x2a),
        &ChecksumCapabilities::default(),
    );
    let ipv4_repr = Ipv4Repr {
        src_addr,
        dst_addr,
        next_header: IpProtocol::Udp,
        hop_limit: 64,
        payload_len: udp_repr.header_len() + PAYLOAD_LEN,
    };

    // Emit to frame
    let mut bytes = vec![0u8; ipv4_repr.buffer_len() + udp_repr.header_len() + PAYLOAD_LEN];
    let frame = {
        ipv4_repr.emit(
            &mut Ipv4Packet::new_unchecked(&mut bytes),
            &ChecksumCapabilities::default(),
        );
        udp_repr.emit(
            &mut UdpPacket::new_unchecked(&mut bytes[ipv4_repr.buffer_len()..]),
            &src_addr.into(),
            &dst_addr.into(),
            PAYLOAD_LEN,
            |buf| fill_slice(buf, 0x2a),
            &ChecksumCapabilities::default(),
        );
        Ipv4Packet::new_unchecked(&bytes[..])
    };

    assert!(iface
        .inner
        .process_ipv4(
            &mut sockets,
            PacketMeta::default(),
            HardwareAddress::default(),
            &frame,
            &mut iface.fragments,
        )
        .is_some());
    assert!(sockets.get_mut::<raw::Socket>(raw_socket_handle).can_recv());
}

#[rstest]
#[case(Medium::Ip)]
#[cfg(all(feature = "socket-raw", feature = "medium-ip"))]
#[case(Medium::Ethernet)]
#[cfg(all(feature = "socket-raw", feature = "medium-ethernet"))]
fn raw_socket_receives_only_locally_routed_ipv4(#[case] medium: Medium) {
    use crate::wire::IpVersion;

    let (mut iface, mut sockets, _) = setup(medium);
    let rx = raw::PacketBuffer::new(vec![raw::PacketMetadata::EMPTY], vec![0; 64]);
    let tx = raw::PacketBuffer::new(vec![raw::PacketMetadata::EMPTY], vec![0; 64]);
    let handle = sockets.add(raw::Socket::new(
        IpVersion::Ipv4,
        IpProtocol::Unknown(253),
        rx,
        tx,
    ));

    let repr = Ipv4Repr {
        src_addr: Ipv4Address::new(127, 0, 0, 2),
        dst_addr: Ipv4Address::new(192, 0, 2, 1),
        next_header: IpProtocol::Unknown(253),
        payload_len: 1,
        hop_limit: 64,
    };
    let mut bytes = vec![0u8; repr.buffer_len() + 1];
    repr.emit(
        &mut Ipv4Packet::new_unchecked(&mut bytes),
        &ChecksumCapabilities::default(),
    );
    bytes[repr.buffer_len()] = 0x42;
    let packet = Ipv4Packet::new_checked(&bytes[..]).unwrap();
    assert!(iface
        .inner
        .process_ipv4(
            &mut sockets,
            PacketMeta::default(),
            HardwareAddress::default(),
            &packet,
            &mut iface.fragments,
        )
        .is_none());
    assert!(!sockets.get_mut::<raw::Socket>(handle).can_recv());

    // AnyIP alone is not enough: the route must resolve through one of this
    // interface's own addresses before raw delivery becomes local delivery.
    iface.set_any_ip(true);
    assert!(iface
        .inner
        .process_ipv4(
            &mut sockets,
            PacketMeta::default(),
            HardwareAddress::default(),
            &packet,
            &mut iface.fragments,
        )
        .is_none());
    assert!(!sockets.get_mut::<raw::Socket>(handle).can_recv());

    iface.routes_mut().update(|routes| {
        let route = crate::iface::Route {
            cidr: IpCidr::new(IpAddress::v4(192, 0, 2, 1), 32),
            via_router: Some(IpAddress::v4(127, 0, 0, 1)),
            preferred_until: None,
            expires_at: None,
        };
        #[cfg(feature = "alloc")]
        routes.push(route);
        #[cfg(not(feature = "alloc"))]
        routes.push(route).unwrap();
    });
    assert!(iface
        .inner
        .process_ipv4(
            &mut sockets,
            PacketMeta::default(),
            HardwareAddress::default(),
            &packet,
            &mut iface.fragments,
        )
        .is_none());
    assert!(sockets.get_mut::<raw::Socket>(handle).can_recv());
}

#[cfg(all(feature = "alloc", feature = "socket-raw", feature = "medium-ip"))]
#[test]
fn filtered_local_input_receives_original_ipv4_and_owns_raw_fanout() {
    struct Capture(Vec<u8>);

    impl IpIngressFilter for Capture {
        fn pre_routing(
            &mut self,
            _: &mut IngressPacket<'_>,
            _: PacketMeta,
            _: HardwareAddress,
        ) -> PreRoutingVerdict {
            PreRoutingVerdict::Pass
        }

        fn local_input(
            &mut self,
            packet: &mut IngressPacket<'_>,
            _: PacketMeta,
            _: HardwareAddress,
            protocol: IpProtocol,
            transport_offset: usize,
        ) -> LocalInputVerdict {
            assert_eq!(protocol, IpProtocol::Unknown(253));
            assert_eq!(transport_offset, 20);
            self.0.extend_from_slice(packet.bytes());
            LocalInputVerdict::ExternalRaw { matched: true }
        }
    }

    let (mut iface, mut sockets, _) = setup(Medium::Ip);
    let rx = raw::PacketBuffer::new(vec![raw::PacketMetadata::EMPTY], vec![0; 64]);
    let tx = raw::PacketBuffer::new(vec![raw::PacketMetadata::EMPTY], vec![0; 64]);
    let handle = sockets.add(raw::Socket::new(
        IpVersion::Ipv4,
        IpProtocol::Unknown(253),
        rx,
        tx,
    ));
    let repr = Ipv4Repr {
        src_addr: Ipv4Address::new(127, 0, 0, 2),
        dst_addr: Ipv4Address::new(127, 0, 0, 1),
        next_header: IpProtocol::Unknown(253),
        payload_len: 1,
        hop_limit: 64,
    };
    let mut bytes = vec![0u8; repr.buffer_len() + 1];
    let mut packet = Ipv4Packet::new_unchecked(&mut bytes[..]);
    repr.emit(&mut packet, &ChecksumCapabilities::default());
    packet.set_dont_frag(true);
    packet.fill_checksum();
    packet.payload_mut()[0] = 0x42;
    let packet = Ipv4Packet::new_checked(&bytes[..]).unwrap();
    let mut scratch = crate::storage::PacketBytes::new();
    let mut filter = Capture(Vec::new());
    assert!(iface
        .inner
        .process_ipv4_filtered(
            &mut sockets,
            PacketMeta::default(),
            HardwareAddress::Ip,
            &packet,
            &mut iface.fragments,
            &mut scratch,
            &mut filter,
        )
        .is_none());
    assert_eq!(filter.0, bytes);
    assert!(
        scratch.is_empty(),
        "read-only LOCAL_IN must not copy the packet"
    );
    assert!(!sockets.get_mut::<raw::Socket>(handle).can_recv());
}

#[cfg(all(
    feature = "proto-ipv4-fragmentation",
    feature = "socket-raw",
    feature = "medium-ip"
))]
#[test]
fn reassembled_ipv4_raw_packet_reports_complete_length() {
    use crate::wire::{IpVersion, UdpPacket, UdpRepr};

    let (mut iface, mut sockets, _) = setup(Medium::Ip);
    let rx = raw::PacketBuffer::new(vec![raw::PacketMetadata::EMPTY], vec![0; 64]);
    let tx = raw::PacketBuffer::new(vec![raw::PacketMetadata::EMPTY], vec![0; 64]);
    let handle = sockets.add(raw::Socket::new(IpVersion::Ipv4, IpProtocol::Udp, rx, tx));

    let src = Ipv4Address::new(127, 0, 0, 2);
    let dst = Ipv4Address::new(127, 0, 0, 1);
    let udp_repr = UdpRepr {
        src_port: 12345,
        dst_port: 23456,
    };
    let mut datagram = [0u8; 24];
    udp_repr.emit(
        &mut UdpPacket::new_unchecked(&mut datagram[..]),
        &src.into(),
        &dst.into(),
        16,
        |payload| payload.fill(0x5a),
        &ChecksumCapabilities::default(),
    );

    for (offset, payload) in [(0, &datagram[..16]), (16, &datagram[16..])] {
        let repr = Ipv4Repr {
            src_addr: src,
            dst_addr: dst,
            next_header: IpProtocol::Udp,
            payload_len: payload.len(),
            hop_limit: if offset == 0 { 64 } else { 1 },
        };
        let mut bytes = vec![0; repr.buffer_len() + payload.len()];
        let mut packet = Ipv4Packet::new_unchecked(&mut bytes[..]);
        repr.emit(&mut packet, &ChecksumCapabilities::default());
        packet.set_ident(42);
        packet.set_frag_offset(offset);
        packet.set_more_frags(offset == 0);
        packet.fill_checksum();
        packet.payload_mut().copy_from_slice(payload);
        let packet = Ipv4Packet::new_checked(&bytes[..]).unwrap();
        iface.inner.process_ipv4(
            &mut sockets,
            PacketMeta::default(),
            HardwareAddress::Ip,
            &packet,
            &mut iface.fragments,
        );
    }

    let packet = sockets.get_mut::<raw::Socket>(handle).recv().unwrap();
    assert_eq!(packet.len(), 20 + datagram.len());
    let header = Ipv4Packet::new_checked(packet).unwrap();
    assert_eq!(header.total_len() as usize, packet.len());
    assert_eq!(header.hop_limit(), 64);
    assert_eq!(header.payload(), datagram);
}

#[cfg(all(
    feature = "proto-ipv4-fragmentation",
    feature = "socket-raw",
    feature = "medium-ip"
))]
#[test]
fn duplicate_final_ipv4_fragment_does_not_complete_reassembly() {
    use crate::wire::{IpVersion, UdpPacket, UdpRepr};

    let (mut iface, mut sockets, _) = setup(Medium::Ip);
    let rx = raw::PacketBuffer::new(vec![raw::PacketMetadata::EMPTY], vec![0; 64]);
    let tx = raw::PacketBuffer::new(vec![raw::PacketMetadata::EMPTY], vec![0; 64]);
    let handle = sockets.add(raw::Socket::new(IpVersion::Ipv4, IpProtocol::Udp, rx, tx));

    let src = Ipv4Address::new(127, 0, 0, 2);
    let dst = Ipv4Address::new(127, 0, 0, 1);
    let mut datagram = [0u8; 24];
    UdpRepr {
        src_port: 12345,
        dst_port: 23456,
    }
    .emit(
        &mut UdpPacket::new_unchecked(&mut datagram[..]),
        &src.into(),
        &dst.into(),
        16,
        |payload| payload.fill(0x5a),
        &ChecksumCapabilities::default(),
    );

    for (offset, payload, more_fragments) in [
        (0, &datagram[..16], true),
        (16, &datagram[16..], true),
        (16, &datagram[16..], false),
    ] {
        let repr = Ipv4Repr {
            src_addr: src,
            dst_addr: dst,
            next_header: IpProtocol::Udp,
            payload_len: payload.len(),
            hop_limit: 64,
        };
        let mut bytes = vec![0; repr.buffer_len() + payload.len()];
        let mut packet = Ipv4Packet::new_unchecked(&mut bytes[..]);
        repr.emit(&mut packet, &ChecksumCapabilities::default());
        packet.set_ident(43);
        packet.set_frag_offset(offset);
        packet.set_more_frags(more_fragments);
        packet.fill_checksum();
        packet.payload_mut().copy_from_slice(payload);
        let packet = Ipv4Packet::new_checked(&bytes[..]).unwrap();
        iface.inner.process_ipv4(
            &mut sockets,
            PacketMeta::default(),
            HardwareAddress::Ip,
            &packet,
            &mut iface.fragments,
        );
        assert!(!sockets.get_mut::<raw::Socket>(handle).can_recv());
    }
}

#[rstest]
#[case(Medium::Ip)]
#[cfg(all(feature = "socket-raw", feature = "socket-udp", feature = "medium-ip"))]
#[case(Medium::Ethernet)]
#[cfg(all(
    feature = "socket-raw",
    feature = "socket-udp",
    feature = "medium-ethernet"
))]
fn test_raw_socket_with_udp_socket(#[case] medium: Medium) {
    use crate::socket::udp;
    use crate::wire::{IpEndpoint, IpVersion, UdpPacket, UdpRepr};

    static UDP_PAYLOAD: [u8; 5] = [0x48, 0x65, 0x6c, 0x6c, 0x6f];

    let (mut iface, mut sockets, _) = setup(medium);

    let udp_rx_buffer = udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY], vec![0; 15]);
    let udp_tx_buffer = udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY], vec![0; 15]);
    let udp_socket = udp::Socket::new(udp_rx_buffer, udp_tx_buffer);
    let udp_socket_handle = sockets.add(udp_socket);

    // Bind the socket to port 68
    let socket = sockets.get_mut::<udp::Socket>(udp_socket_handle);
    assert_eq!(socket.bind(68), Ok(()));
    assert!(!socket.can_recv());
    assert!(socket.can_send());

    let packets = 1;
    let raw_rx_buffer =
        raw::PacketBuffer::new(vec![raw::PacketMetadata::EMPTY; packets], vec![0; 48 * 1]);
    let raw_tx_buffer = raw::PacketBuffer::new(
        vec![raw::PacketMetadata::EMPTY; packets],
        vec![0; 48 * packets],
    );
    let raw_socket = raw::Socket::new(
        IpVersion::Ipv4,
        IpProtocol::Udp,
        raw_rx_buffer,
        raw_tx_buffer,
    );
    sockets.add(raw_socket);

    let src_addr = Ipv4Address::new(127, 0, 0, 2);
    let dst_addr = Ipv4Address::new(127, 0, 0, 1);

    let udp_repr = UdpRepr {
        src_port: 67,
        dst_port: 68,
    };
    let mut bytes = vec![0xff; udp_repr.header_len() + UDP_PAYLOAD.len()];
    let mut packet = UdpPacket::new_unchecked(&mut bytes[..]);
    udp_repr.emit(
        &mut packet,
        &src_addr.into(),
        &dst_addr.into(),
        UDP_PAYLOAD.len(),
        |buf| buf.copy_from_slice(&UDP_PAYLOAD),
        &ChecksumCapabilities::default(),
    );
    let ipv4_repr = Ipv4Repr {
        src_addr,
        dst_addr,
        next_header: IpProtocol::Udp,
        hop_limit: 64,
        payload_len: udp_repr.header_len() + UDP_PAYLOAD.len(),
    };

    // Emit to frame
    let mut bytes = vec![0u8; ipv4_repr.buffer_len() + udp_repr.header_len() + UDP_PAYLOAD.len()];
    let frame = {
        ipv4_repr.emit(
            &mut Ipv4Packet::new_unchecked(&mut bytes),
            &ChecksumCapabilities::default(),
        );
        udp_repr.emit(
            &mut UdpPacket::new_unchecked(&mut bytes[ipv4_repr.buffer_len()..]),
            &src_addr.into(),
            &dst_addr.into(),
            UDP_PAYLOAD.len(),
            |buf| buf.copy_from_slice(&UDP_PAYLOAD),
            &ChecksumCapabilities::default(),
        );
        Ipv4Packet::new_unchecked(&bytes[..])
    };

    assert_eq!(
        iface.inner.process_ipv4(
            &mut sockets,
            PacketMeta::default(),
            HardwareAddress::default(),
            &frame,
            &mut iface.fragments
        ),
        None
    );

    // Make sure the UDP socket can still receive in presence of a Raw socket that handles UDP
    let socket = sockets.get_mut::<udp::Socket>(udp_socket_handle);
    assert!(socket.can_recv());
    assert_eq!(
        socket.recv(),
        Ok((
            &UDP_PAYLOAD[..],
            udp::UdpMetadata {
                local_address: Some(dst_addr.into()),
                local_port: Some(68),
                ..IpEndpoint::new(src_addr.into(), 67).into()
            }
        ))
    );
}

#[rstest]
#[case(Medium::Ip)]
#[cfg(all(feature = "socket-udp", feature = "medium-ip"))]
#[case(Medium::Ethernet)]
#[cfg(all(feature = "socket-udp", feature = "medium-ethernet"))]
fn test_icmp_reply_size(#[case] medium: Medium) {
    use crate::wire::IPV4_MIN_MTU as MIN_MTU;
    const MAX_PAYLOAD_LEN: usize = 528;

    let (mut iface, mut sockets, _device) = setup(medium);

    let src_addr = Ipv4Address::new(192, 168, 1, 1);
    let dst_addr = Ipv4Address::new(192, 168, 1, 2);

    // UDP packet that if not tructated will cause a icmp port unreachable reply
    // to exceed the minimum mtu bytes in length.
    let udp_repr = UdpRepr {
        src_port: 67,
        dst_port: 68,
    };
    let mut bytes = vec![0xff; udp_repr.header_len() + MAX_PAYLOAD_LEN];
    let mut packet = UdpPacket::new_unchecked(&mut bytes[..]);
    udp_repr.emit(
        &mut packet,
        &src_addr.into(),
        &dst_addr.into(),
        MAX_PAYLOAD_LEN,
        |buf| fill_slice(buf, 0x2a),
        &ChecksumCapabilities::default(),
    );

    let ip_repr = Ipv4Repr {
        src_addr,
        dst_addr,
        next_header: IpProtocol::Udp,
        hop_limit: 64,
        payload_len: udp_repr.header_len() + MAX_PAYLOAD_LEN,
    };
    let payload = packet.into_inner();

    let expected_icmp_repr = Icmpv4Repr::DstUnreachable {
        reason: Icmpv4DstUnreachable::PortUnreachable,
        header: ip_repr,
        data: &payload[..MAX_PAYLOAD_LEN],
    };

    let expected_ip_repr = Ipv4Repr {
        src_addr: dst_addr,
        dst_addr: src_addr,
        next_header: IpProtocol::Icmp,
        hop_limit: 64,
        payload_len: expected_icmp_repr.buffer_len(),
    };

    assert_eq!(
        expected_ip_repr.buffer_len() + expected_icmp_repr.buffer_len(),
        MIN_MTU
    );

    assert_eq!(
        iface.inner.process_udp(
            &mut sockets,
            PacketMeta::default(),
            ip_repr.into(),
            payload,
            false
        ),
        Some(Packet::new_ipv4(
            expected_ip_repr,
            IpPayload::Icmpv4(expected_icmp_repr)
        ))
    );
}

#[test]
#[cfg(all(feature = "proto-ipv4-fragmentation", feature = "medium-ip"))]
fn exact_buffer_failure_preserves_ipv4_emission_and_fragment_progress() {
    use core::cell::{Cell, RefCell};
    use std::rc::Rc;

    struct PreparedTx {
        fail: bool,
        prepared: Rc<Cell<usize>>,
        emitted: Rc<RefCell<Vec<Vec<u8>>>>,
    }
    impl TxToken for PreparedTx {
        fn prepare_buffer(&mut self, len: usize) -> Result<(), crate::phy::TxEgressError> {
            self.prepared.set(len);
            if self.fail {
                Err(crate::phy::TxEgressError::Exhausted)
            } else {
                Ok(())
            }
        }
        fn consume<R, F>(self, len: usize, emit: F) -> R
        where
            F: FnOnce(&mut [u8]) -> R,
        {
            assert!(!self.fail);
            assert_eq!(self.prepared.get(), len);
            let mut bytes = vec![0; len];
            let result = emit(&mut bytes);
            self.emitted.borrow_mut().push(bytes);
            result
        }
    }
    for fragmented in [false, true] {
        let (mut iface, _, _) = setup(Medium::Ip);
        let payload = vec![
            0xa5;
            if fragmented {
                iface.inner.ip_mtu() + 256
            } else {
                64
            }
        ];
        let packet = || {
            Packet::new_ipv4(
                Ipv4Repr {
                    src_addr: Ipv4Address::new(192, 0, 2, 1),
                    dst_addr: Ipv4Address::new(198, 51, 100, 1),
                    next_header: IpProtocol::Udp,
                    payload_len: payload.len(),
                    hop_limit: 64,
                },
                IpPayload::Raw(&payload),
            )
        };
        let prepared = Rc::new(Cell::new(0));
        let emitted = Rc::new(RefCell::new(Vec::new()));
        let token = |fail| PreparedTx {
            fail,
            prepared: prepared.clone(),
            emitted: emitted.clone(),
        };
        let initial = (iface.fragmenter.packet_len, iface.fragmenter.sent_bytes);
        assert_eq!(
            iface.inner.dispatch_ip(
                token(true),
                PacketMeta::default(),
                packet(),
                &mut iface.fragmenter
            ),
            Err(DispatchError::Exhausted)
        );
        assert!(emitted.borrow().is_empty());
        assert_eq!(
            (iface.fragmenter.packet_len, iface.fragmenter.sent_bytes),
            initial
        );
        iface
            .inner
            .dispatch_ip(
                token(false),
                PacketMeta::default(),
                packet(),
                &mut iface.fragmenter,
            )
            .unwrap();
        if fragmented {
            let before = (
                iface.fragmenter.sent_bytes,
                iface.fragmenter.ipv4.frag_offset,
                iface.fragmenter.ipv4.repr.payload_len,
            );
            iface
                .inner
                .dispatch_ipv4_frag(token(true), &mut iface.fragmenter);
            assert_eq!(emitted.borrow().len(), 1);
            assert_eq!(
                (
                    iface.fragmenter.sent_bytes,
                    iface.fragmenter.ipv4.frag_offset,
                    iface.fragmenter.ipv4.repr.payload_len
                ),
                before
            );
            while !iface.fragmenter.finished() {
                iface
                    .inner
                    .dispatch_ipv4_frag(token(false), &mut iface.fragmenter);
            }
        }
        let mut recovered = Vec::new();
        for frame in emitted.borrow().iter() {
            let ip = Ipv4Packet::new_checked(&frame[..]).unwrap();
            assert_eq!(ip.frag_offset() as usize, recovered.len());
            recovered.extend_from_slice(ip.payload());
        }
        assert_eq!(recovered, payload);
    }
}

#[test]
#[cfg(all(
    feature = "alloc",
    feature = "proto-ipv4-fragmentation",
    feature = "medium-ip"
))]
fn fragment_storage_failure_never_acknowledges_unsent_payload() {
    use core::cell::Cell;
    use std::rc::Rc;
    fn fail(
        _: usize,
    ) -> Result<crate::storage::PacketBytesStorage, crate::storage::OwnedBufferResizeError> {
        Err(crate::storage::OwnedBufferResizeError)
    }
    struct Tx(Rc<Cell<usize>>);
    impl TxToken for Tx {
        fn consume<R, F>(self, len: usize, emit: F) -> R
        where
            F: FnOnce(&mut [u8]) -> R,
        {
            self.0.set(self.0.get() + 1);
            emit(&mut vec![0; len])
        }
    }
    let (mut iface, _, _) = setup(Medium::Ip);
    iface.set_packet_buffer_allocator(fail).unwrap();
    let payload = vec![0xa5; iface.inner.ip_mtu() + 256];
    let packet = || {
        Packet::new_ipv4(
            Ipv4Repr {
                src_addr: Ipv4Address::new(192, 0, 2, 1),
                dst_addr: Ipv4Address::new(198, 51, 100, 1),
                next_header: IpProtocol::Udp,
                payload_len: payload.len(),
                hop_limit: 64,
            },
            IpPayload::Raw(&payload),
        )
    };
    let emitted = Rc::new(Cell::new(0));
    let before = (
        iface.fragmenter.packet_len,
        iface.fragmenter.sent_bytes,
        iface.fragmenter.ipv4.frag_offset,
    );
    assert_eq!(
        iface.inner.dispatch_ip(
            Tx(emitted.clone()),
            PacketMeta::default(),
            packet(),
            &mut iface.fragmenter
        ),
        Err(DispatchError::Exhausted)
    );
    assert_eq!(emitted.get(), 0);
    assert_eq!(
        (
            iface.fragmenter.packet_len,
            iface.fragmenter.sent_bytes,
            iface.fragmenter.ipv4.frag_offset
        ),
        before
    );
    iface.fragmenter.buffer = crate::storage::PacketBytes::new();
    assert_eq!(
        iface.inner.dispatch_ip(
            Tx(emitted.clone()),
            PacketMeta::default(),
            packet(),
            &mut iface.fragmenter
        ),
        Ok(())
    );
    assert_eq!(emitted.get(), 1);
    while !iface.fragmenter.is_empty() && !iface.fragmenter.finished() {
        iface
            .inner
            .dispatch_ipv4_frag(Tx(emitted.clone()), &mut iface.fragmenter);
    }
    assert!(emitted.get() >= 2);
}
