use super::*;

#[cfg(all(feature = "alloc", feature = "medium-ethernet", feature = "socket-raw"))]
#[test]
fn deferred_ipv6_output_serializes_without_neighbor_lookup() {
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
            _: Option<u16>,
            emit: F,
        ) -> Result<(), crate::phy::IpOutputError>
        where
            F: FnOnce(&mut [u8]),
        {
            assert_eq!(class, crate::phy::IpOutputClass::Ordinary);
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
    let payload = [0x5a; 32];
    let source = Ipv6Address::new(0xfd00, 0, 0, 0, 0, 0, 0, 1);
    let destination = Ipv6Address::new(0xfd01, 0, 0, 0, 0, 0, 0, 7);
    let packet = Packet::new_ipv6(
        Ipv6Repr {
            src_addr: source,
            dst_addr: destination,
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
    let ipv6 = Ipv6Packet::new_checked(&output[..]).unwrap();
    assert_eq!(ipv6.payload_len() as usize + 40, output.len());
    assert_eq!(ipv6.dst_addr(), destination);
    assert_eq!(ipv6.payload(), &payload[..]);
}

#[cfg(all(feature = "alloc", feature = "medium-ip"))]
struct RewriteIpv6Destination {
    calls: usize,
}

#[cfg(all(feature = "alloc", feature = "medium-ip"))]
struct CountValidatedIpv6 {
    observed: Vec<Vec<u8>>,
}

#[cfg(all(feature = "alloc", feature = "medium-ip"))]
impl IpIngressFilter for CountValidatedIpv6 {
    fn pre_routing(
        &mut self,
        packet: &mut IngressPacket<'_>,
        _: PacketMeta,
        _: HardwareAddress,
    ) -> PreRoutingVerdict {
        self.observed.push(packet.take_owned().unwrap().to_vec());
        PreRoutingVerdict::Drop
    }
}

#[cfg(all(feature = "alloc", feature = "medium-ip", feature = "packetmeta-id"))]
#[test]
fn ipv6_pre_routing_receives_rx_token_metadata() {
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
    device.rx_queue.push_back(routed_ipv6_bytes());
    let mut filter = CaptureMeta(None);
    let meta = PacketMeta { id: 72 };
    device.rx_meta = meta;
    assert_eq!(
        iface.poll_ingress_single_filtered(Instant::ZERO, &mut device, &mut sockets, &mut filter),
        PollIngressSingleResult::SocketStateChanged
    );
    assert_eq!(filter.0, Some(meta));
}

#[cfg(all(feature = "alloc", feature = "medium-ip"))]
#[test]
fn ipv6_receive_validation_precedes_filter_callback() {
    let (mut iface, mut sockets, _) = setup(Medium::Ip);
    let mut scratch = crate::storage::PacketBytes::new();
    let mut filter = CountValidatedIpv6 {
        observed: Vec::new(),
    };

    let mut invalid_version = routed_ipv6_bytes();
    invalid_version[0] = 0x40;
    let invalid_version = Ipv6Packet::new_checked(&invalid_version[..]).unwrap();
    assert!(iface
        .inner
        .process_ipv6_filtered(
            &mut sockets,
            PacketMeta::default(),
            HardwareAddress::Ip,
            &invalid_version,
            &mut scratch,
            &mut filter,
        )
        .is_none());

    let mut unsupported_jumbo = routed_ipv6_bytes();
    unsupported_jumbo[6] = 0; // Hop-by-Hop with zero fixed-header payload length.
    unsupported_jumbo.extend_from_slice(&[59, 0, 0xc2, 4, 0, 1, 0, 0]);
    let unsupported_jumbo = Ipv6Packet::new_checked(&unsupported_jumbo[..]).unwrap();
    assert!(iface
        .inner
        .process_ipv6_filtered(
            &mut sockets,
            PacketMeta::default(),
            HardwareAddress::Ip,
            &unsupported_jumbo,
            &mut scratch,
            &mut filter,
        )
        .is_none());

    let mut discarded_option = routed_ipv6_bytes();
    discarded_option[5] = 8;
    discarded_option[6] = 0;
    discarded_option.extend_from_slice(&[59, 0, 0x40, 0, 0, 0, 0, 0]);
    let discarded_option = Ipv6Packet::new_checked(&discarded_option[..]).unwrap();
    assert!(iface
        .inner
        .process_ipv6_filtered(
            &mut sockets,
            PacketMeta::default(),
            HardwareAddress::Ip,
            &discarded_option,
            &mut scratch,
            &mut filter,
        )
        .is_none());
    assert!(filter.observed.is_empty());
}

#[cfg(all(feature = "alloc", feature = "medium-ip"))]
#[test]
fn ipv6_discard_option_after_four_padding_options_is_not_observed() {
    let (mut iface, mut sockets, _) = setup(Medium::Ip);
    let mut scratch = crate::storage::PacketBytes::new();
    let mut filter = CountValidatedIpv6 {
        observed: Vec::new(),
    };
    let mut bytes = routed_ipv6_bytes();
    bytes[5] = 8;
    bytes[6] = 0;
    bytes.extend_from_slice(&[59, 0, 0, 0, 0, 0, 0x40, 0]);
    let packet = Ipv6Packet::new_checked(&bytes[..]).unwrap();
    assert!(iface
        .inner
        .process_ipv6_filtered(
            &mut sockets,
            PacketMeta::default(),
            HardwareAddress::Ip,
            &packet,
            &mut scratch,
            &mut filter,
        )
        .is_none());
    assert!(filter.observed.is_empty());

    let mut valid = routed_ipv6_bytes();
    valid[5] = 8;
    valid[6] = 0;
    valid.extend_from_slice(&[59, 0, 0, 0, 0, 0, 0, 0]);
    let valid_packet = Ipv6Packet::new_checked(&valid[..]).unwrap();
    assert!(iface
        .inner
        .process_ipv6_filtered(
            &mut sockets,
            PacketMeta::default(),
            HardwareAddress::Ip,
            &valid_packet,
            &mut scratch,
            &mut filter,
        )
        .is_none());
    assert_eq!(filter.observed, vec![valid]);
}

#[cfg(all(feature = "alloc", feature = "medium-ip"))]
#[test]
fn non_loopback_ip_medium_rejects_ipv6_loopback_and_interface_local() {
    let (mut iface, mut sockets, _) = setup(Medium::Ip);
    let mut scratch = crate::storage::PacketBytes::new();
    let mut filter = CountValidatedIpv6 {
        observed: Vec::new(),
    };
    for destination in [
        Ipv6Address::LOCALHOST,
        Ipv6Address::new(0xff01, 0, 0, 0, 0, 0, 0, 1),
    ] {
        let mut bytes = routed_ipv6_bytes();
        Ipv6Packet::new_unchecked(&mut bytes).set_dst_addr(destination);
        let packet = Ipv6Packet::new_checked(&bytes[..]).unwrap();
        assert!(iface
            .inner
            .process_ipv6_filtered(
                &mut sockets,
                PacketMeta::default(),
                HardwareAddress::Ip,
                &packet,
                &mut scratch,
                &mut filter,
            )
            .is_none());
    }
    assert!(filter.observed.is_empty());

    iface.inner.is_loopback = true;
    let mut bytes = routed_ipv6_bytes();
    Ipv6Packet::new_unchecked(&mut bytes).set_dst_addr(Ipv6Address::LOCALHOST);
    let packet = Ipv6Packet::new_checked(&bytes[..]).unwrap();
    assert!(iface
        .inner
        .process_ipv6_filtered(
            &mut sockets,
            PacketMeta::default(),
            HardwareAddress::Ip,
            &packet,
            &mut scratch,
            &mut filter,
        )
        .is_none());
    assert_eq!(filter.observed, vec![bytes]);
}

#[cfg(all(feature = "alloc", feature = "medium-ip", feature = "medium-ethernet"))]
#[test]
fn ethernet_ipv6_invalid_scopes_never_reach_filter_callback() {
    let (mut iface, mut sockets, _) = setup(Medium::Ethernet);
    let mut scratch = crate::storage::PacketBytes::new();
    let mut filter = CountValidatedIpv6 {
        observed: Vec::new(),
    };
    let loopback = Ipv6Address::LOCALHOST;
    let outside = Ipv6Address::new(0xfd01, 0, 0, 0, 0, 0, 0, 7);
    let cases = [
        (loopback, outside),
        (Ipv6Address::new(0xfd00, 0, 0, 0, 0, 0, 0, 1), loopback),
        (
            Ipv6Address::new(0xfd00, 0, 0, 0, 0, 0, 0, 1),
            Ipv6Address::new(0xff00, 0, 0, 0, 0, 0, 0, 1),
        ),
        (
            Ipv6Address::new(0xfd00, 0, 0, 0, 0, 0, 0, 1),
            Ipv6Address::new(0xff01, 0, 0, 0, 0, 0, 0, 1),
        ),
    ];
    for (src, dst) in cases {
        let mut bytes = routed_ipv6_bytes();
        let mut packet = Ipv6Packet::new_unchecked(&mut bytes);
        packet.set_src_addr(src);
        packet.set_dst_addr(dst);
        let packet = Ipv6Packet::new_checked(&bytes[..]).unwrap();
        assert!(iface
            .inner
            .process_ipv6_filtered(
                &mut sockets,
                PacketMeta::default(),
                HardwareAddress::Ethernet(EthernetAddress([2, 0, 0, 0, 0, 9])),
                &packet,
                &mut scratch,
                &mut filter,
            )
            .is_none());
    }
    assert!(filter.observed.is_empty());
}

#[cfg(all(feature = "alloc", feature = "medium-ip"))]
impl IpIngressFilter for RewriteIpv6Destination {
    fn pre_routing(
        &mut self,
        packet: &mut IngressPacket<'_>,
        _: PacketMeta,
        _: HardwareAddress,
    ) -> PreRoutingVerdict {
        self.calls += 1;
        assert_eq!(packet.bytes().len(), 48);
        let mut ipv6 = Ipv6Packet::new_unchecked(packet.writable().unwrap());
        let destination = Ipv6Address::new(0xfdbe, 0, 0, 0, 0, 0, 0, 1);
        let source = ipv6.src_addr();
        ipv6.set_dst_addr(destination);
        Icmpv6Packet::new_unchecked(ipv6.payload_mut()).fill_checksum(&source, &destination);
        PreRoutingVerdict::Pass
    }
}

#[cfg(all(feature = "alloc", feature = "medium-ip"))]
#[test]
fn full_poll_applies_ipv6_rewrite_before_local_delivery() {
    let (mut iface, mut sockets, mut device) = setup(Medium::Ip);
    let remote = Ipv6Address::new(0xfdbe, 0, 0, 0, 0, 0, 0, 2);
    let original_destination = Ipv6Address::new(0xfdbe, 0, 0, 0, 0, 0, 0, 3);
    let echo = Icmpv6Repr::EchoRequest {
        ident: 7,
        seq_no: 1,
        data: &[],
    };
    let repr = Ipv6Repr {
        src_addr: remote,
        dst_addr: original_destination,
        next_header: IpProtocol::Icmpv6,
        payload_len: echo.buffer_len(),
        hop_limit: 64,
    };
    let mut bytes = vec![0; repr.buffer_len() + echo.buffer_len() + 8];
    repr.emit(&mut Ipv6Packet::new_unchecked(&mut bytes));
    echo.emit(
        &remote,
        &original_destination,
        &mut Icmpv6Packet::new_unchecked(&mut bytes[40..48]),
        &ChecksumCapabilities::default(),
    );
    device.rx_queue.push_back(bytes);
    let mut filter = RewriteIpv6Destination { calls: 0 };
    assert_eq!(
        iface.poll_filtered(Instant::ZERO, &mut device, &mut sockets, &mut filter),
        PollResult::SocketStateChanged
    );
    assert_eq!(filter.calls, 1);
    let reply = device.tx_queue.pop_front().unwrap();
    let ip = Ipv6Packet::new_checked(&reply[..]).unwrap();
    assert_eq!(ip.src_addr(), Ipv6Address::new(0xfdbe, 0, 0, 0, 0, 0, 0, 1));
    assert_eq!(ip.dst_addr(), remote);
}

#[cfg(all(feature = "alloc", feature = "medium-ip"))]
#[test]
fn local_input_ipv6_rewrite_is_validated_before_transport_reply() {
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
            assert_eq!(protocol, IpProtocol::Icmpv6);
            assert_eq!(transport_offset, 40);
            let mut ipv6 = Ipv6Packet::new_unchecked(packet.writable().unwrap());
            let source = Ipv6Address::new(0xfdbe, 0, 0, 0, 0, 0, 0, 9);
            let destination = ipv6.dst_addr();
            ipv6.set_src_addr(source);
            if self.repair_checksum {
                Icmpv6Packet::new_unchecked(ipv6.payload_mut())
                    .fill_checksum(&source, &destination);
            }
            LocalInputVerdict::Pass
        }
    }

    let (mut iface, mut sockets, _) = setup(Medium::Ip);
    let old_source = Ipv6Address::new(0xfdbe, 0, 0, 0, 0, 0, 0, 2);
    let new_source = Ipv6Address::new(0xfdbe, 0, 0, 0, 0, 0, 0, 9);
    let destination = Ipv6Address::new(0xfdbe, 0, 0, 0, 0, 0, 0, 1);
    let echo = Icmpv6Repr::EchoRequest {
        ident: 7,
        seq_no: 1,
        data: &[1, 2, 3, 4],
    };
    let repr = Ipv6Repr {
        src_addr: old_source,
        dst_addr: destination,
        next_header: IpProtocol::Icmpv6,
        payload_len: echo.buffer_len(),
        hop_limit: 64,
    };
    let mut bytes = vec![0; repr.buffer_len() + echo.buffer_len()];
    repr.emit(&mut Ipv6Packet::new_unchecked(&mut bytes));
    echo.emit(
        &old_source,
        &destination,
        &mut Icmpv6Packet::new_unchecked(&mut bytes[40..]),
        &ChecksumCapabilities::default(),
    );
    let original = bytes.clone();
    let packet = Ipv6Packet::new_checked(&bytes[..]).unwrap();
    let mut scratch = crate::storage::PacketBytes::new();
    let mut filter = RewriteSource {
        calls: 0,
        repair_checksum: true,
    };
    let reply = iface.inner.process_ipv6_filtered(
        &mut sockets,
        PacketMeta::default(),
        HardwareAddress::Ip,
        &packet,
        &mut scratch,
        &mut filter,
    );
    let expected = Packet::new_ipv6(
        Ipv6Repr {
            src_addr: destination,
            dst_addr: new_source,
            next_header: IpProtocol::Icmpv6,
            payload_len: echo.buffer_len(),
            hop_limit: 64,
        },
        IpPayload::Icmpv6(Icmpv6Repr::EchoReply {
            ident: 7,
            seq_no: 1,
            data: &[1, 2, 3, 4],
        }),
    );
    assert_eq!(reply, Some(expected));
    assert_eq!(filter.calls, 1);
    assert_eq!(bytes, original);
    drop(reply);
    assert_eq!(&scratch[8..24], &new_source.octets());

    let mut malformed = RewriteSource {
        calls: 0,
        repair_checksum: false,
    };
    assert!(iface
        .inner
        .process_ipv6_filtered(
            &mut sockets,
            PacketMeta::default(),
            HardwareAddress::Ip,
            &packet,
            &mut scratch,
            &mut malformed,
        )
        .is_none());
    assert_eq!(malformed.calls, 1);
}

#[cfg(all(feature = "alloc", feature = "medium-ip"))]
#[test]
fn local_input_ipv6_cannot_rewrite_destination_to_nonlocal() {
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
            let mut ipv6 = Ipv6Packet::new_unchecked(packet.writable().unwrap());
            let source = ipv6.src_addr();
            let destination = Ipv6Address::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 9);
            ipv6.set_dst_addr(destination);
            Icmpv6Packet::new_unchecked(ipv6.payload_mut()).fill_checksum(&source, &destination);
            LocalInputVerdict::Pass
        }
    }

    let (mut iface, mut sockets, _) = setup(Medium::Ip);
    let source = Ipv6Address::new(0xfdbe, 0, 0, 0, 0, 0, 0, 2);
    let destination = Ipv6Address::new(0xfdbe, 0, 0, 0, 0, 0, 0, 1);
    let echo = Icmpv6Repr::EchoRequest {
        ident: 7,
        seq_no: 1,
        data: &[],
    };
    let repr = Ipv6Repr {
        src_addr: source,
        dst_addr: destination,
        next_header: IpProtocol::Icmpv6,
        payload_len: echo.buffer_len(),
        hop_limit: 64,
    };
    let mut bytes = vec![0; repr.buffer_len() + echo.buffer_len()];
    repr.emit(&mut Ipv6Packet::new_unchecked(&mut bytes));
    echo.emit(
        &source,
        &destination,
        &mut Icmpv6Packet::new_unchecked(&mut bytes[40..]),
        &ChecksumCapabilities::default(),
    );
    let packet = Ipv6Packet::new_checked(&bytes[..]).unwrap();
    let mut scratch = crate::storage::PacketBytes::new();
    assert!(iface
        .inner
        .process_ipv6_filtered(
            &mut sockets,
            PacketMeta::default(),
            HardwareAddress::Ip,
            &packet,
            &mut scratch,
            &mut RewriteDestination,
        )
        .is_none());
}

#[derive(Clone)]
struct CapturedIpv6(std::rc::Rc<core::cell::RefCell<Vec<Vec<u8>>>>);

impl TxToken for CapturedIpv6 {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
        let mut bytes = vec![0; len];
        let result = f(&mut bytes);
        self.0.borrow_mut().push(bytes);
        result
    }
}

fn routed_ipv6_bytes() -> Vec<u8> {
    let repr = Ipv6Repr {
        src_addr: Ipv6Address::new(0xfd00, 0, 0, 0, 0, 0, 0, 1),
        dst_addr: Ipv6Address::new(0xfd01, 0, 0, 0, 0, 0, 0, 7),
        next_header: IpProtocol::Ipv6NoNxt,
        payload_len: 0,
        hop_limit: 64,
    };
    let mut bytes = vec![0; 40];
    repr.emit(&mut Ipv6Packet::new_unchecked(&mut bytes));
    bytes
}

#[cfg(all(feature = "alloc", feature = "medium-ip"))]
#[test]
fn ipv6_packet_aware_filter_selects_fragments_without_slowing_plain_packets() {
    struct SelectFragments {
        calls: usize,
    }

    impl IpIngressFilter for SelectFragments {
        fn applies_to(&self, _: IpVersion) -> bool {
            false
        }

        fn applies_to_packet(&self, version: IpVersion, packet: &[u8]) -> bool {
            version == IpVersion::Ipv6 && packet.get(6) == Some(&44)
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

    let (mut iface, mut sockets, _) = setup(Medium::Ip);
    let mut scratch = crate::storage::PacketBytes::new();
    let mut filter = SelectFragments { calls: 0 };
    let normal = routed_ipv6_bytes();
    let _ = iface.inner.process_ip_filtered(
        &mut sockets,
        PacketMeta::default(),
        &normal,
        &mut iface.fragments,
        &mut scratch,
        &mut filter,
    );
    assert_eq!(filter.calls, 0);

    let mut fragment = routed_ipv6_bytes();
    fragment[6] = 44;
    fragment[5] = 8;
    fragment.extend_from_slice(&[59, 0, 0, 0, 0, 0, 0, 1]);
    let _ = iface.inner.process_ip_filtered(
        &mut sockets,
        PacketMeta::default(),
        &fragment,
        &mut iface.fragments,
        &mut scratch,
        &mut filter,
    );
    assert_eq!(filter.calls, 1);
}

#[cfg(all(feature = "alloc", feature = "medium-ip"))]
#[test]
fn ipv6_defrag_callback_runs_before_ordinary_pre_routing() {
    struct Capture {
        stages: Vec<&'static str>,
    }

    impl IpIngressFilter for Capture {
        fn pre_routing_ipv6_defrag(
            &mut self,
            packet: &mut IngressPacket<'_>,
            _: PacketMeta,
            _: HardwareAddress,
        ) -> PreRoutingVerdict {
            self.stages.push("defrag");
            if packet.bytes()[6] == 44 {
                let owned = packet.take_owned().unwrap();
                assert_eq!(owned[6], 44);
                PreRoutingVerdict::Drop
            } else {
                PreRoutingVerdict::Pass
            }
        }

        fn pre_routing(
            &mut self,
            _: &mut IngressPacket<'_>,
            _: PacketMeta,
            _: HardwareAddress,
        ) -> PreRoutingVerdict {
            self.stages.push("pre_routing");
            PreRoutingVerdict::Drop
        }
    }

    let (mut iface, mut sockets, _) = setup(Medium::Ip);
    let mut scratch = crate::storage::PacketBytes::new();
    let mut filter = Capture { stages: Vec::new() };
    let normal = routed_ipv6_bytes();
    let normal = Ipv6Packet::new_checked(&normal[..]).unwrap();
    assert!(iface
        .inner
        .process_ipv6_filtered(
            &mut sockets,
            PacketMeta::default(),
            HardwareAddress::Ip,
            &normal,
            &mut scratch,
            &mut filter,
        )
        .is_none());
    assert_eq!(filter.stages, ["defrag", "pre_routing"]);

    filter.stages.clear();
    let mut fragment = routed_ipv6_bytes();
    fragment[6] = 44;
    fragment[5] = 8;
    fragment.extend_from_slice(&[59, 0, 0, 0, 0, 0, 0, 1]);
    let fragment = Ipv6Packet::new_checked(&fragment[..]).unwrap();
    assert!(iface
        .inner
        .process_ipv6_filtered(
            &mut sockets,
            PacketMeta::default(),
            HardwareAddress::Ip,
            &fragment,
            &mut scratch,
            &mut filter,
        )
        .is_none());
    assert_eq!(filter.stages, ["defrag"]);
}

#[cfg(all(feature = "alloc", feature = "medium-ip"))]
#[test]
fn ipv6_route_handoff_runs_after_prerouting_and_before_local_fanout() {
    struct Capture {
        pre_routing: bool,
        transferred: Vec<u8>,
    }

    impl IpIngressFilter for Capture {
        fn pre_routing(
            &mut self,
            _: &mut IngressPacket<'_>,
            _: PacketMeta,
            _: HardwareAddress,
        ) -> PreRoutingVerdict {
            self.pre_routing = true;
            PreRoutingVerdict::Pass
        }

        fn route_input(
            &mut self,
            packet: &mut RoutedIngressPacket<'_, '_>,
            _: PacketMeta,
            _: HardwareAddress,
        ) -> RouteInputVerdict {
            assert!(self.pre_routing);
            self.transferred = packet.take_owned().unwrap().to_vec();
            RouteInputVerdict::Forward
        }

        fn local_input(
            &mut self,
            _: &mut IngressPacket<'_>,
            _: PacketMeta,
            _: HardwareAddress,
            _: IpProtocol,
            _: usize,
        ) -> LocalInputVerdict {
            panic!("forwarded IPv6 must never enter local raw or transport fanout")
        }
    }

    let (mut iface, mut sockets, _) = setup(Medium::Ip);
    let bytes = routed_ipv6_bytes();
    let packet = Ipv6Packet::new_checked(&bytes[..]).unwrap();
    let mut scratch = crate::storage::PacketBytes::new();
    let mut filter = Capture {
        pre_routing: false,
        transferred: Vec::new(),
    };
    assert!(iface
        .inner
        .process_ipv6_filtered(
            &mut sockets,
            PacketMeta::default(),
            HardwareAddress::Ip,
            &packet,
            &mut scratch,
            &mut filter,
        )
        .is_none());
    assert_eq!(filter.transferred, bytes);
}

#[test]
#[cfg(feature = "medium-ethernet")]
fn global_source_ns_reply_uses_ingress_link_without_source_route() {
    let (mut iface, mut sockets, _) = setup(Medium::Ethernet);
    let local = Ipv6Address::new(0xfdbe, 0, 0, 0, 0, 0, 0, 1);
    // Deliberately outside every configured prefix, with no default route.
    let remote = Ipv6Address::new(0x2001, 0xdb8, 0x1234, 0, 0, 0, 0, 9);
    let remote_mac = EthernetAddress([2, 0, 0, 0, 0, 9]);
    let solicitation = Icmpv6Repr::Ndisc(NdiscRepr::NeighborSolicit {
        target_addr: local,
        lladdr: Some(remote_mac.into()),
    });
    let ip = Ipv6Repr {
        src_addr: remote,
        dst_addr: local.solicited_node(),
        next_header: IpProtocol::Icmpv6,
        payload_len: solicitation.buffer_len(),
        hop_limit: 255,
    };
    let mut bytes = vec![0; 14 + 40 + solicitation.buffer_len()];
    let mut frame = EthernetFrame::new_unchecked(&mut bytes);
    frame.set_src_addr(remote_mac);
    frame.set_dst_addr(EthernetAddress([0x33, 0x33, 0xff, 0, 0, 1]));
    frame.set_ethertype(EthernetProtocol::Ipv6);
    ip.emit(&mut Ipv6Packet::new_unchecked(frame.payload_mut()));
    solicitation.emit(
        &remote,
        &ip.dst_addr,
        &mut Icmpv6Packet::new_unchecked(&mut frame.payload_mut()[40..]),
        &ChecksumCapabilities::default(),
    );
    let Some(EthernetPacket::Ip(reply)) = iface.inner.process_ethernet(
        &mut sockets,
        PacketMeta::default(),
        frame.into_inner(),
        &mut iface.fragments,
    ) else {
        panic!("valid NS must generate a neighbor advertisement");
    };
    // Learning a neighbor is not a route: ordinary IP output still fails.
    assert_eq!(
        iface
            .inner
            .lookup_hardware_addr(MockTxToken, &remote.into(), None, &mut iface.fragmenter),
        Err(DispatchError::NoRoute)
    );
    let frames = std::rc::Rc::new(core::cell::RefCell::new(Vec::new()));
    assert_eq!(
        iface.inner.dispatch_ip(
            CapturedIpv6(frames.clone()),
            PacketMeta::default(),
            reply,
            &mut iface.fragmenter,
        ),
        Ok(())
    );
    let captured = frames.borrow();
    assert_eq!(captured.len(), 1);
    let frame = EthernetFrame::new_checked(&captured[0]).unwrap();
    assert_eq!(frame.dst_addr(), remote_mac);
    let packet = Ipv6Packet::new_checked(frame.payload()).unwrap();
    assert_eq!(packet.dst_addr(), remote);
    assert_eq!(packet.hop_limit(), 255);
    let icmp = Icmpv6Packet::new_checked(packet.payload()).unwrap();
    assert!(matches!(
        Icmpv6Repr::parse(&local, &remote, &icmp, &ChecksumCapabilities::default()).unwrap(),
        Icmpv6Repr::Ndisc(NdiscRepr::NeighborAdvert { target_addr, .. }) if target_addr == local
    ));
    drop(captured);
    let echo = Icmpv6Repr::EchoReply {
        ident: 1,
        seq_no: 1,
        data: &[],
    };
    assert_eq!(
        iface.inner.dispatch_ip(
            CapturedIpv6(frames.clone()),
            PacketMeta::default(),
            Packet::new_ipv6(
                Ipv6Repr {
                    src_addr: local,
                    dst_addr: remote,
                    next_header: IpProtocol::Icmpv6,
                    hop_limit: 64,
                    payload_len: echo.buffer_len(),
                },
                IpPayload::Icmpv6(echo)
            ),
            &mut iface.fragmenter,
        ),
        Err(DispatchError::NoRoute)
    );
    assert_eq!(frames.borrow().len(), 1);
}

#[test]
#[cfg(feature = "medium-ethernet")]
fn explicit_ipv6_dispatch_preserves_packet_and_next_hop() {
    let (mut iface, _, _) = setup(Medium::Ethernet);
    let frames = std::rc::Rc::new(core::cell::RefCell::new(Vec::new()));
    let mac = EthernetAddress([2, 0, 0, 0, 0, 9]);
    let hop = Ipv6Address::new(0xfd00, 0, 0, 0, 0, 0, 0, 9).into();
    let bytes = routed_ipv6_bytes();
    assert_eq!(
        iface.dispatch_ip_packet(
            Instant::ZERO,
            CapturedIpv6(frames.clone()),
            hop,
            Some(HardwareAddress::Ethernet(mac)),
            &bytes
        ),
        Ok(())
    );
    let frames = frames.borrow();
    let frame = EthernetFrame::new_checked(&frames[0]).unwrap();
    assert_eq!(frame.ethertype(), EthernetProtocol::Ipv6);
    assert_eq!(frame.dst_addr(), mac);
    assert_eq!(frame.payload(), &bytes);
}

#[test]
#[cfg(feature = "medium-ethernet")]
fn explicit_ipv6_dispatch_discovers_and_retries_neighbor() {
    let (mut iface, _, _) = setup(Medium::Ethernet);
    let frames = std::rc::Rc::new(core::cell::RefCell::new(Vec::new()));
    let hop = Ipv6Address::new(0xfdbe, 0, 0, 0, 0, 0, 0, 9);
    let bytes = routed_ipv6_bytes();
    for _ in 0..2 {
        assert_eq!(
            iface.dispatch_ip_packet(
                Instant::ZERO,
                CapturedIpv6(frames.clone()),
                hop.into(),
                None,
                &bytes
            ),
            Err(IpPacketDispatchError::NeighborPending {
                retry_at: Instant::from_millis(1000)
            })
        );
    }
    assert_eq!(frames.borrow().len(), 1);
    {
        let frames = frames.borrow();
        let frame = EthernetFrame::new_checked(&frames[0]).unwrap();
        let ip = Ipv6Packet::new_checked(frame.payload()).unwrap();
        assert_eq!(ip.hop_limit(), 255);
        assert_eq!(ip.dst_addr(), hop.solicited_node());
        assert_eq!(
            Icmpv6Packet::new_checked(ip.payload()).unwrap().msg_type(),
            Icmpv6Message::NeighborSolicit
        );
    }
    iface.inner.neighbor_cache.fill(
        hop.into(),
        HardwareAddress::Ethernet(EthernetAddress([2, 0, 0, 0, 0, 9])),
        Instant::ZERO,
    );
    assert_eq!(
        iface.dispatch_ip_packet(
            Instant::ZERO,
            CapturedIpv6(frames.clone()),
            hop.into(),
            None,
            &bytes
        ),
        Ok(())
    );
    assert_eq!(frames.borrow().len(), 2);
}

#[test]
#[cfg(feature = "medium-ip")]
fn explicit_ipv6_dispatch_validates_complete_packet() {
    let (mut iface, _, _) = setup(Medium::Ip);
    let frames = std::rc::Rc::new(core::cell::RefCell::new(Vec::new()));
    let hop = Ipv6Address::LOCALHOST.into();
    let mut bytes = routed_ipv6_bytes();
    assert_eq!(
        iface.dispatch_ip_packet(
            Instant::ZERO,
            CapturedIpv6(frames.clone()),
            hop,
            None,
            &bytes
        ),
        Ok(())
    );
    assert_eq!(frames.borrow()[0], bytes);
    bytes.push(0);
    assert_eq!(
        iface.dispatch_ip_packet(
            Instant::ZERO,
            CapturedIpv6(frames.clone()),
            hop,
            None,
            &bytes
        ),
        Err(IpPacketDispatchError::Malformed)
    );
    assert_eq!(
        iface.dispatch_ip_packet(
            Instant::ZERO,
            CapturedIpv6(frames.clone()),
            hop,
            None,
            &bytes[..10]
        ),
        Err(IpPacketDispatchError::Malformed)
    );
    #[cfg(feature = "proto-ipv4")]
    assert_eq!(
        iface.dispatch_ip_packet(
            Instant::ZERO,
            CapturedIpv6(frames.clone()),
            Ipv4Address::LOCALHOST.into(),
            None,
            &bytes[..40]
        ),
        Err(IpPacketDispatchError::Malformed)
    );
    assert_eq!(frames.borrow().len(), 1);
}

#[test]
#[cfg(all(feature = "medium-ip", feature = "socket-raw"))]
fn ipv6_rejects_oversized_route_before_consuming_token() {
    struct SmallRoute;
    impl TxToken for SmallRoute {
        fn egress_override(
            &mut self,
            _: IpVersion,
            _: IpAddress,
            _: PacketMeta,
        ) -> Result<Option<crate::phy::TxEgressOverride>, crate::phy::TxEgressError> {
            Ok(Some(crate::phy::TxEgressOverride {
                medium: Medium::Ip,
                ip_mtu: 1280,
                context: [0; 3],
            }))
        }
        fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, _: usize, _: F) -> R {
            panic!("oversized IPv6 must not consume a device buffer");
        }
    }
    let (mut iface, _, _) = setup(Medium::Ip);
    let data = [0; 1280];
    let packet = Packet::new_ipv6(
        Ipv6Repr {
            src_addr: Ipv6Address::LOCALHOST,
            dst_addr: Ipv6Address::new(0xfd00, 0, 0, 0, 0, 0, 0, 9),
            next_header: IpProtocol::Udp,
            payload_len: data.len(),
            hop_limit: 64,
        },
        IpPayload::Raw(&data),
    );
    assert_eq!(
        iface.inner.dispatch_ip(
            SmallRoute,
            PacketMeta::default(),
            packet,
            &mut iface.fragmenter
        ),
        Err(DispatchError::NoRoute)
    );
}

#[test]
#[cfg(all(feature = "medium-ip", feature = "medium-ethernet"))]
fn ipv6_ndisc_does_not_query_external_egress() {
    struct LinkOnly;
    impl TxToken for LinkOnly {
        fn egress_override(
            &mut self,
            _: IpVersion,
            _: IpAddress,
            _: PacketMeta,
        ) -> Result<Option<crate::phy::TxEgressOverride>, crate::phy::TxEgressError> {
            panic!("NDP must stay on its physical link");
        }
        fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
            f(&mut vec![0; len])
        }
    }
    let (mut iface, _, _) = setup(Medium::Ip);
    let addr = Ipv6Address::new(0xfd00, 0, 0, 0, 0, 0, 0, 9);
    let icmp = Icmpv6Repr::Ndisc(NdiscRepr::NeighborAdvert {
        flags: NdiscNeighborFlags::SOLICITED,
        target_addr: addr,
        lladdr: None,
    });
    let packet = Packet::new_ipv6(
        Ipv6Repr {
            src_addr: Ipv6Address::LOCALHOST,
            dst_addr: addr,
            next_header: IpProtocol::Icmpv6,
            payload_len: icmp.buffer_len(),
            hop_limit: 255,
        },
        IpPayload::Icmpv6(icmp),
    );
    assert_eq!(
        iface.inner.dispatch_ip(
            LinkOnly,
            PacketMeta::default(),
            packet,
            &mut iface.fragmenter
        ),
        Ok(())
    );
}

fn parse_ipv6(data: &[u8]) -> crate::wire::Result<Packet<'_>> {
    let ipv6_header = Ipv6Packet::new_checked(data)?;
    let ipv6 = Ipv6Repr::parse(&ipv6_header)?;

    match ipv6.next_header {
        IpProtocol::HopByHop => todo!(),
        IpProtocol::Icmp => todo!(),
        IpProtocol::Igmp => todo!(),
        IpProtocol::Tcp => todo!(),
        IpProtocol::Udp => todo!(),
        IpProtocol::Ipv6Route => todo!(),
        IpProtocol::Ipv6Frag => todo!(),
        IpProtocol::IpSecEsp => todo!(),
        IpProtocol::IpSecAh => todo!(),
        IpProtocol::Icmpv6 => {
            let icmp = Icmpv6Repr::parse(
                &ipv6.src_addr,
                &ipv6.dst_addr,
                &Icmpv6Packet::new_checked(ipv6_header.payload())?,
                &Default::default(),
            )?;
            Ok(Packet::new_ipv6(ipv6, IpPayload::Icmpv6(icmp)))
        }
        IpProtocol::Ipv6NoNxt => todo!(),
        IpProtocol::Ipv6Opts => todo!(),
        IpProtocol::Unknown(_) => todo!(),
    }
}

#[rstest]
#[case::ip(Medium::Ip)]
#[cfg(feature = "medium-ip")]
#[case::ethernet(Medium::Ethernet)]
#[cfg(feature = "medium-ethernet")]
#[case::ieee802154(Medium::Ieee802154)]
#[cfg(feature = "medium-ieee802154")]
fn any_ip(#[case] medium: Medium) {
    // An empty echo request with destination address fdbe::3, which is not part of the interface
    // address list.
    let data = [
        0x60, 0x0, 0x0, 0x0, 0x0, 0x8, 0x3a, 0x40, 0xfd, 0xbe, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x2, 0xfd, 0xbe, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x3, 0x80, 0x0, 0x84, 0x3a, 0x0, 0x0, 0x0, 0x0,
    ];

    assert_eq!(
        parse_ipv6(&data),
        Ok(Packet::new_ipv6(
            Ipv6Repr {
                src_addr: Ipv6Address::new(0xfdbe, 0, 0, 0, 0, 0, 0, 0x0002),
                dst_addr: Ipv6Address::new(0xfdbe, 0, 0, 0, 0, 0, 0, 0x0003),
                hop_limit: 64,
                next_header: IpProtocol::Icmpv6,
                payload_len: 8,
            },
            IpPayload::Icmpv6(Icmpv6Repr::EchoRequest {
                ident: 0,
                seq_no: 0,
                data: b"",
            })
        ))
    );

    let (mut iface, mut sockets, _device) = setup(medium);

    // Add a route to the interface, otherwise, we don't know if the packet is routed localy.
    iface.routes_mut().update(|routes| {
        let route = crate::iface::Route {
            cidr: IpCidr::Ipv6(Ipv6Cidr::new(
                Ipv6Address::new(0xfdbe, 0, 0, 0, 0, 0, 0, 0),
                64,
            )),
            via_router: Some(IpAddress::Ipv6(Ipv6Address::new(
                0xfdbe, 0, 0, 0, 0, 0, 0, 0x0001,
            ))),
            preferred_until: None,
            expires_at: None,
        };
        #[cfg(feature = "alloc")]
        routes.push(route);
        #[cfg(not(feature = "alloc"))]
        routes.push(route).unwrap();
    });

    assert_eq!(
        iface.inner.process_ipv6(
            &mut sockets,
            PacketMeta::default(),
            HardwareAddress::default(),
            &Ipv6Packet::new_checked(&data[..]).unwrap()
        ),
        None
    );

    // Accept any IP:
    iface.set_any_ip(true);
    assert!(iface
        .inner
        .process_ipv6(
            &mut sockets,
            PacketMeta::default(),
            HardwareAddress::default(),
            &Ipv6Packet::new_checked(&data[..]).unwrap()
        )
        .is_some());
}

#[rstest]
#[case::ip(Medium::Ip)]
#[cfg(feature = "medium-ip")]
#[case::ethernet(Medium::Ethernet)]
#[cfg(feature = "medium-ethernet")]
#[case::ieee802154(Medium::Ieee802154)]
#[cfg(feature = "medium-ieee802154")]
fn multicast_source_address(#[case] medium: Medium) {
    let data = [
        0x60, 0x0, 0x0, 0x0, 0x0, 0x0, 0xc, 0x40, 0xff, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x1, 0xfd, 0xbe, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x1,
    ];

    let response = None;

    let (mut iface, mut sockets, _device) = setup(medium);

    assert_eq!(
        iface.inner.process_ipv6(
            &mut sockets,
            PacketMeta::default(),
            HardwareAddress::default(),
            &Ipv6Packet::new_checked(&data[..]).unwrap()
        ),
        response
    );
}

#[rstest]
#[case::ip(Medium::Ip)]
#[cfg(feature = "medium-ip")]
#[case::ethernet(Medium::Ethernet)]
#[cfg(feature = "medium-ethernet")]
#[case::ieee802154(Medium::Ieee802154)]
#[cfg(feature = "medium-ieee802154")]
fn hop_by_hop_skip_with_icmp(#[case] medium: Medium) {
    // The following contains:
    // - IPv6 header
    // - Hop-by-hop, with options:
    //  - PADN (skipped)
    //  - Unknown option (skipped)
    // - ICMP echo request
    let data = [
        0x60, 0x0, 0x0, 0x0, 0x0, 0x1b, 0x0, 0x40, 0xfd, 0xbe, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x2, 0xfd, 0xbe, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x1, 0x3a, 0x0, 0x1, 0x0, 0xf, 0x0, 0x1, 0x0, 0x80, 0x0, 0x2c, 0x88,
        0x0, 0x2a, 0x1, 0xa4, 0x4c, 0x6f, 0x72, 0x65, 0x6d, 0x20, 0x49, 0x70, 0x73, 0x75, 0x6d,
    ];

    let response = Some(Packet::new_ipv6(
        Ipv6Repr {
            src_addr: Ipv6Address::new(0xfdbe, 0, 0, 0, 0, 0, 0, 0x0001),
            dst_addr: Ipv6Address::new(0xfdbe, 0, 0, 0, 0, 0, 0, 0x0002),
            hop_limit: 64,
            next_header: IpProtocol::Icmpv6,
            payload_len: 19,
        },
        IpPayload::Icmpv6(Icmpv6Repr::EchoReply {
            ident: 42,
            seq_no: 420,
            data: b"Lorem Ipsum",
        }),
    ));

    let (mut iface, mut sockets, _device) = setup(medium);

    assert_eq!(
        iface.inner.process_ipv6(
            &mut sockets,
            PacketMeta::default(),
            HardwareAddress::default(),
            &Ipv6Packet::new_checked(&data[..]).unwrap()
        ),
        response
    );
}

#[rstest]
#[case::ip(Medium::Ip)]
#[cfg(feature = "medium-ip")]
#[case::ethernet(Medium::Ethernet)]
#[cfg(feature = "medium-ethernet")]
#[case::ieee802154(Medium::Ieee802154)]
#[cfg(feature = "medium-ieee802154")]
fn hop_by_hop_discard_with_icmp(#[case] medium: Medium) {
    // The following contains:
    // - IPv6 header
    // - Hop-by-hop, with options:
    //  - PADN (skipped)
    //  - Unknown option (discard)
    // - ICMP echo request
    let data = [
        0x60, 0x0, 0x0, 0x0, 0x0, 0x1b, 0x0, 0x40, 0xfd, 0xbe, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x2, 0xfd, 0xbe, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x1, 0x3a, 0x0, 0x1, 0x0, 0x40, 0x0, 0x1, 0x0, 0x80, 0x0, 0x2c, 0x88,
        0x0, 0x2a, 0x1, 0xa4, 0x4c, 0x6f, 0x72, 0x65, 0x6d, 0x20, 0x49, 0x70, 0x73, 0x75, 0x6d,
    ];

    let response = None;

    let (mut iface, mut sockets, _device) = setup(medium);

    assert_eq!(
        iface.inner.process_ipv6(
            &mut sockets,
            PacketMeta::default(),
            HardwareAddress::default(),
            &Ipv6Packet::new_checked(&data[..]).unwrap()
        ),
        response
    );
}

#[rstest]
#[case::ip(Medium::Ip)]
#[cfg(feature = "medium-ip")]
fn hop_by_hop_discard_param_problem(#[case] medium: Medium) {
    // The following contains:
    // - IPv6 header
    // - Hop-by-hop, with options:
    //  - PADN (skipped)
    //  - Unknown option (discard + ParamProblem)
    // - ICMP echo request
    let data = [
        0x60, 0x0, 0x0, 0x0, 0x0, 0x1b, 0x0, 0x40, 0xfd, 0xbe, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x2, 0xfd, 0xbe, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x1, 0x3a, 0x0, 0xC0, 0x0, 0x40, 0x0, 0x1, 0x0, 0x80, 0x0, 0x2c, 0x88,
        0x0, 0x2a, 0x1, 0xa4, 0x4c, 0x6f, 0x72, 0x65, 0x6d, 0x20, 0x49, 0x70, 0x73, 0x75, 0x6d,
    ];

    let response = Some(Packet::new_ipv6(
        Ipv6Repr {
            src_addr: Ipv6Address::new(0xfdbe, 0, 0, 0, 0, 0, 0, 1),
            dst_addr: Ipv6Address::new(0xfdbe, 0, 0, 0, 0, 0, 0, 2),
            next_header: IpProtocol::Icmpv6,
            payload_len: 75,
            hop_limit: 64,
        },
        IpPayload::Icmpv6(Icmpv6Repr::ParamProblem {
            reason: Icmpv6ParamProblem::UnrecognizedOption,
            pointer: 40,
            header: Ipv6Repr {
                src_addr: Ipv6Address::new(0xfdbe, 0, 0, 0, 0, 0, 0, 2),
                dst_addr: Ipv6Address::new(0xfdbe, 0, 0, 0, 0, 0, 0, 1),
                next_header: IpProtocol::HopByHop,
                payload_len: 27,
                hop_limit: 64,
            },
            data: &[
                0x3a, 0x0, 0xC0, 0x0, 0x40, 0x0, 0x1, 0x0, 0x80, 0x0, 0x2c, 0x88, 0x0, 0x2a, 0x1,
                0xa4, 0x4c, 0x6f, 0x72, 0x65, 0x6d, 0x20, 0x49, 0x70, 0x73, 0x75, 0x6d,
            ],
        }),
    ));

    let (mut iface, mut sockets, _device) = setup(medium);

    assert_eq!(
        iface.inner.process_ipv6(
            &mut sockets,
            PacketMeta::default(),
            HardwareAddress::default(),
            &Ipv6Packet::new_checked(&data[..]).unwrap()
        ),
        response
    );
}

#[rstest]
#[case::ip(Medium::Ip)]
#[cfg(feature = "medium-ip")]
fn hop_by_hop_discard_with_multicast(#[case] medium: Medium) {
    // The following contains:
    // - IPv6 header
    // - Hop-by-hop, with options:
    //  - PADN (skipped)
    //  - Unknown option (discard (0b11) + ParamProblem)
    // - ICMP echo request
    //
    // In this case, even if the destination address is a multicast address, an ICMPv6 ParamProblem
    // should be transmitted.
    let data = [
        0x60, 0x0, 0x0, 0x0, 0x0, 0x1b, 0x0, 0x40, 0xfd, 0xbe, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x2, 0xff, 0x02, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x1, 0x3a, 0x0, 0x80, 0x0, 0x40, 0x0, 0x1, 0x0, 0x80, 0x0, 0x2c, 0x88,
        0x0, 0x2a, 0x1, 0xa4, 0x4c, 0x6f, 0x72, 0x65, 0x6d, 0x20, 0x49, 0x70, 0x73, 0x75, 0x6d,
    ];

    let response = Some(Packet::new_ipv6(
        Ipv6Repr {
            src_addr: Ipv6Address::new(0xfdbe, 0, 0, 0, 0, 0, 0, 1),
            dst_addr: Ipv6Address::new(0xfdbe, 0, 0, 0, 0, 0, 0, 2),
            next_header: IpProtocol::Icmpv6,
            payload_len: 75,
            hop_limit: 64,
        },
        IpPayload::Icmpv6(Icmpv6Repr::ParamProblem {
            reason: Icmpv6ParamProblem::UnrecognizedOption,
            pointer: 40,
            header: Ipv6Repr {
                src_addr: Ipv6Address::new(0xfdbe, 0, 0, 0, 0, 0, 0, 2),
                dst_addr: Ipv6Address::new(0xff02, 0, 0, 0, 0, 0, 0, 1),
                next_header: IpProtocol::HopByHop,
                payload_len: 27,
                hop_limit: 64,
            },
            data: &[
                0x3a, 0x0, 0x80, 0x0, 0x40, 0x0, 0x1, 0x0, 0x80, 0x0, 0x2c, 0x88, 0x0, 0x2a, 0x1,
                0xa4, 0x4c, 0x6f, 0x72, 0x65, 0x6d, 0x20, 0x49, 0x70, 0x73, 0x75, 0x6d,
            ],
        }),
    ));

    let (mut iface, mut sockets, _device) = setup(medium);

    assert_eq!(
        iface.inner.process_ipv6(
            &mut sockets,
            PacketMeta::default(),
            HardwareAddress::default(),
            &Ipv6Packet::new_checked(&data[..]).unwrap()
        ),
        response
    );
}

#[rstest]
#[case::ip(Medium::Ip)]
#[cfg(feature = "medium-ip")]
#[case::ethernet(Medium::Ethernet)]
#[cfg(feature = "medium-ethernet")]
#[case::ieee802154(Medium::Ieee802154)]
#[cfg(feature = "medium-ieee802154")]
fn imcp_empty_echo_request(#[case] medium: Medium) {
    let data = [
        0x60, 0x0, 0x0, 0x0, 0x0, 0x8, 0x3a, 0x40, 0xfd, 0xbe, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x2, 0xfd, 0xbe, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x1, 0x80, 0x0, 0x84, 0x3c, 0x0, 0x0, 0x0, 0x0,
    ];

    assert_eq!(
        parse_ipv6(&data),
        Ok(Packet::new_ipv6(
            Ipv6Repr {
                src_addr: Ipv6Address::new(0xfdbe, 0, 0, 0, 0, 0, 0, 0x0002),
                dst_addr: Ipv6Address::new(0xfdbe, 0, 0, 0, 0, 0, 0, 0x0001),
                hop_limit: 64,
                next_header: IpProtocol::Icmpv6,
                payload_len: 8,
            },
            IpPayload::Icmpv6(Icmpv6Repr::EchoRequest {
                ident: 0,
                seq_no: 0,
                data: b"",
            })
        ))
    );

    let response = Some(Packet::new_ipv6(
        Ipv6Repr {
            src_addr: Ipv6Address::new(0xfdbe, 0, 0, 0, 0, 0, 0, 0x0001),
            dst_addr: Ipv6Address::new(0xfdbe, 0, 0, 0, 0, 0, 0, 0x0002),
            hop_limit: 64,
            next_header: IpProtocol::Icmpv6,
            payload_len: 8,
        },
        IpPayload::Icmpv6(Icmpv6Repr::EchoReply {
            ident: 0,
            seq_no: 0,
            data: b"",
        }),
    ));

    let (mut iface, mut sockets, _device) = setup(medium);

    assert_eq!(
        iface.inner.process_ipv6(
            &mut sockets,
            PacketMeta::default(),
            HardwareAddress::default(),
            &Ipv6Packet::new_checked(&data[..]).unwrap()
        ),
        response
    );
}

#[rstest]
#[case::ip(Medium::Ip)]
#[cfg(feature = "medium-ip")]
#[case::ethernet(Medium::Ethernet)]
#[cfg(feature = "medium-ethernet")]
#[case::ieee802154(Medium::Ieee802154)]
#[cfg(feature = "medium-ieee802154")]
fn icmp_echo_request(#[case] medium: Medium) {
    let data = [
        0x60, 0x0, 0x0, 0x0, 0x0, 0x13, 0x3a, 0x40, 0xfd, 0xbe, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x2, 0xfd, 0xbe, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x1, 0x80, 0x0, 0x2c, 0x88, 0x0, 0x2a, 0x1, 0xa4, 0x4c, 0x6f, 0x72,
        0x65, 0x6d, 0x20, 0x49, 0x70, 0x73, 0x75, 0x6d,
    ];

    assert_eq!(
        parse_ipv6(&data),
        Ok(Packet::new_ipv6(
            Ipv6Repr {
                src_addr: Ipv6Address::new(0xfdbe, 0, 0, 0, 0, 0, 0, 0x0002),
                dst_addr: Ipv6Address::new(0xfdbe, 0, 0, 0, 0, 0, 0, 0x0001),
                hop_limit: 64,
                next_header: IpProtocol::Icmpv6,
                payload_len: 19,
            },
            IpPayload::Icmpv6(Icmpv6Repr::EchoRequest {
                ident: 42,
                seq_no: 420,
                data: b"Lorem Ipsum",
            })
        ))
    );

    let response = Some(Packet::new_ipv6(
        Ipv6Repr {
            src_addr: Ipv6Address::new(0xfdbe, 0, 0, 0, 0, 0, 0, 0x0001),
            dst_addr: Ipv6Address::new(0xfdbe, 0, 0, 0, 0, 0, 0, 0x0002),
            hop_limit: 64,
            next_header: IpProtocol::Icmpv6,
            payload_len: 19,
        },
        IpPayload::Icmpv6(Icmpv6Repr::EchoReply {
            ident: 42,
            seq_no: 420,
            data: b"Lorem Ipsum",
        }),
    ));

    let (mut iface, mut sockets, _device) = setup(medium);

    assert_eq!(
        iface.inner.process_ipv6(
            &mut sockets,
            PacketMeta::default(),
            HardwareAddress::default(),
            &Ipv6Packet::new_checked(&data[..]).unwrap()
        ),
        response
    );
}

#[rstest]
#[case::ip(Medium::Ip)]
#[cfg(feature = "medium-ip")]
#[case::ethernet(Medium::Ethernet)]
#[cfg(feature = "medium-ethernet")]
#[case::ieee802154(Medium::Ieee802154)]
#[cfg(feature = "medium-ieee802154")]
fn icmp_echo_reply_as_input(#[case] medium: Medium) {
    let data = [
        0x60, 0x0, 0x0, 0x0, 0x0, 0x13, 0x3a, 0x40, 0xfd, 0xbe, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x2, 0xfd, 0xbe, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x1, 0x81, 0x0, 0x2d, 0x56, 0x0, 0x0, 0x0, 0x0, 0x4c, 0x6f, 0x72, 0x65,
        0x6d, 0x20, 0x49, 0x70, 0x73, 0x75, 0x6d,
    ];

    assert_eq!(
        parse_ipv6(&data),
        Ok(Packet::new_ipv6(
            Ipv6Repr {
                src_addr: Ipv6Address::new(0xfdbe, 0, 0, 0, 0, 0, 0, 0x0002),
                dst_addr: Ipv6Address::new(0xfdbe, 0, 0, 0, 0, 0, 0, 0x0001),
                hop_limit: 64,
                next_header: IpProtocol::Icmpv6,
                payload_len: 19,
            },
            IpPayload::Icmpv6(Icmpv6Repr::EchoReply {
                ident: 0,
                seq_no: 0,
                data: b"Lorem Ipsum",
            })
        ))
    );

    let response = None;

    let (mut iface, mut sockets, _device) = setup(medium);

    assert_eq!(
        iface.inner.process_ipv6(
            &mut sockets,
            PacketMeta::default(),
            HardwareAddress::default(),
            &Ipv6Packet::new_checked(&data[..]).unwrap()
        ),
        response
    );
}

#[rstest]
#[case::ip(Medium::Ip)]
#[cfg(feature = "medium-ip")]
#[case::ethernet(Medium::Ethernet)]
#[cfg(feature = "medium-ethernet")]
#[case::ieee802154(Medium::Ieee802154)]
#[cfg(feature = "medium-ieee802154")]
fn unknown_proto_with_multicast_dst_address(#[case] medium: Medium) {
    let data = [
        0x60, 0x0, 0x0, 0x0, 0x0, 0x0, 0xc, 0x40, 0xfd, 0xbe, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x2, 0xff, 0x2, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x1,
    ];

    let response = Some(Packet::new_ipv6(
        Ipv6Repr {
            src_addr: Ipv6Address::new(0xfdbe, 0, 0, 0, 0, 0, 0, 0x0001),
            dst_addr: Ipv6Address::new(0xfdbe, 0, 0, 0, 0, 0, 0, 0x0002),
            hop_limit: 64,
            next_header: IpProtocol::Icmpv6,
            payload_len: 48,
        },
        IpPayload::Icmpv6(Icmpv6Repr::ParamProblem {
            reason: Icmpv6ParamProblem::UnrecognizedNxtHdr,
            pointer: 40,
            header: Ipv6Repr {
                src_addr: Ipv6Address::new(0xfdbe, 0, 0, 0, 0, 0, 0, 0x0002),
                dst_addr: Ipv6Address::new(0xff02, 0, 0, 0, 0, 0, 0, 0x0001),
                hop_limit: 64,
                next_header: IpProtocol::Unknown(0x0c),
                payload_len: 0,
            },
            data: &[],
        }),
    ));

    let (mut iface, mut sockets, _device) = setup(medium);

    assert_eq!(
        iface.inner.process_ipv6(
            &mut sockets,
            PacketMeta::default(),
            HardwareAddress::default(),
            &Ipv6Packet::new_checked(&data[..]).unwrap()
        ),
        response
    );
}

#[rstest]
#[case::ip(Medium::Ip)]
#[cfg(feature = "medium-ip")]
#[case::ethernet(Medium::Ethernet)]
#[cfg(feature = "medium-ethernet")]
#[case::ieee802154(Medium::Ieee802154)]
#[cfg(feature = "medium-ieee802154")]
fn unknown_proto(#[case] medium: Medium) {
    // Since the destination address is multicast, we should answer with an ICMPv6 message.
    let data = [
        0x60, 0x0, 0x0, 0x0, 0x0, 0x0, 0xc, 0x40, 0xfd, 0xbe, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x2, 0xfd, 0xbe, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x1,
    ];

    let response = Some(Packet::new_ipv6(
        Ipv6Repr {
            src_addr: Ipv6Address::new(0xfdbe, 0, 0, 0, 0, 0, 0, 0x0001),
            dst_addr: Ipv6Address::new(0xfdbe, 0, 0, 0, 0, 0, 0, 0x0002),
            hop_limit: 64,
            next_header: IpProtocol::Icmpv6,
            payload_len: 48,
        },
        IpPayload::Icmpv6(Icmpv6Repr::ParamProblem {
            reason: Icmpv6ParamProblem::UnrecognizedNxtHdr,
            pointer: 40,
            header: Ipv6Repr {
                src_addr: Ipv6Address::new(0xfdbe, 0, 0, 0, 0, 0, 0, 0x0002),
                dst_addr: Ipv6Address::new(0xfdbe, 0, 0, 0, 0, 0, 0, 0x0001),
                hop_limit: 64,
                next_header: IpProtocol::Unknown(0x0c),
                payload_len: 0,
            },
            data: &[],
        }),
    ));

    let (mut iface, mut sockets, _device) = setup(medium);

    assert_eq!(
        iface.inner.process_ipv6(
            &mut sockets,
            PacketMeta::default(),
            HardwareAddress::default(),
            &Ipv6Packet::new_checked(&data[..]).unwrap()
        ),
        response
    );
}

#[rstest]
#[case::ethernet(Medium::Ethernet)]
#[cfg(feature = "medium-ethernet")]
fn ndisc_neighbor_advertisement_ethernet(#[case] medium: Medium) {
    let data = [
        0x60, 0x0, 0x0, 0x0, 0x0, 0x20, 0x3a, 0xff, 0xfd, 0xbe, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x2, 0xfd, 0xbe, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x1, 0x88, 0x0, 0x3b, 0x9f, 0x40, 0x0, 0x0, 0x0, 0xfe, 0x80, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x2, 0x2, 0x1, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x1,
    ];

    assert_eq!(
        parse_ipv6(&data),
        Ok(Packet::new_ipv6(
            Ipv6Repr {
                src_addr: Ipv6Address::new(0xfdbe, 0, 0, 0, 0, 0, 0, 0x0002),
                dst_addr: Ipv6Address::new(0xfdbe, 0, 0, 0, 0, 0, 0, 0x0001),
                hop_limit: 255,
                next_header: IpProtocol::Icmpv6,
                payload_len: 32,
            },
            IpPayload::Icmpv6(Icmpv6Repr::Ndisc(NdiscRepr::NeighborAdvert {
                flags: NdiscNeighborFlags::SOLICITED,
                target_addr: Ipv6Address::new(0xfe80, 0, 0, 0, 0, 0, 0, 0x0002),
                lladdr: Some(RawHardwareAddress::from_bytes(&[0, 0, 0, 0, 0, 1])),
            }))
        ))
    );

    let response = None;

    let (mut iface, mut sockets, _device) = setup(medium);

    iface.set_neighbor_discovery_enabled(false);
    assert_eq!(
        iface.inner.process_ipv6(
            &mut sockets,
            PacketMeta::default(),
            HardwareAddress::default(),
            &Ipv6Packet::new_checked(&data[..]).unwrap()
        ),
        None
    );
    iface.set_neighbor_discovery_enabled(true);
    assert_eq!(
        iface.inner.neighbor_cache.lookup(
            &IpAddress::Ipv6(Ipv6Address::new(0xfdbe, 0, 0, 0, 0, 0, 0, 0x0002)),
            iface.inner.now,
        ),
        NeighborAnswer::NotFound,
    );

    assert_eq!(
        iface.inner.process_ipv6(
            &mut sockets,
            PacketMeta::default(),
            HardwareAddress::default(),
            &Ipv6Packet::new_checked(&data[..]).unwrap()
        ),
        response
    );

    assert_eq!(
        iface.inner.neighbor_cache.lookup(
            &IpAddress::Ipv6(Ipv6Address::new(0xfdbe, 0, 0, 0, 0, 0, 0, 0x0002)),
            iface.inner.now,
        ),
        NeighborAnswer::Found(HardwareAddress::Ethernet(EthernetAddress::from_bytes(&[
            0, 0, 0, 0, 0, 1
        ]))),
    );
}

#[cfg(all(feature = "alloc", feature = "medium-ethernet"))]
#[test]
fn routed_link_control_updates_only_the_receiving_interfaces_neighbor_cache() {
    struct KeepOnIngress {
        local_input_calls: usize,
    }

    impl IpIngressFilter for KeepOnIngress {
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
            _: &mut RoutedIngressPacket<'_, '_>,
            _: PacketMeta,
            _: HardwareAddress,
        ) -> RouteInputVerdict {
            RouteInputVerdict::NeighborDiscovery
        }

        fn local_input(
            &mut self,
            _: &mut IngressPacket<'_>,
            _: PacketMeta,
            _: HardwareAddress,
            _: IpProtocol,
            _: usize,
        ) -> LocalInputVerdict {
            self.local_input_calls += 1;
            LocalInputVerdict::Pass
        }
    }

    // The destination is not assigned to this interface. This is the normal
    // weak-host case where a different interface owns the global address.
    let src = Ipv6Address::new(0xfdbe, 0, 0, 0, 0, 0, 0, 2);
    let dst = Ipv6Address::new(0xfdbe, 0, 0, 0, 0, 0, 0, 3);
    let advertised_mac = EthernetAddress([2, 0, 0, 0, 0, 9]);
    let mut data = [0; 72];
    data[0] = 0x60;
    data[4..6].copy_from_slice(&32u16.to_be_bytes());
    data[6] = u8::from(IpProtocol::Icmpv6);
    data[7] = 255;
    data[40] = 136; // Neighbor Advertisement.
    data[44] = 0x40; // Solicited.
    data[48..64].copy_from_slice(&Ipv6Address::new(0xfe80, 0, 0, 0, 0, 0, 0, 2).octets());
    data[64] = 2; // Target Link-Layer Address option.
    data[65] = 1; // One eight-byte unit.
    data[66..72].copy_from_slice(&advertised_mac.0);
    {
        let mut ip = Ipv6Packet::new_unchecked(&mut data[..]);
        ip.set_src_addr(src);
        ip.set_dst_addr(dst);
    }
    Icmpv6Packet::new_unchecked(&mut data[40..]).fill_checksum(&src, &dst);

    let (mut iface, mut sockets, _) = setup(Medium::Ethernet);
    let mut scratch = crate::storage::PacketBytes::new();
    let mut filter = KeepOnIngress {
        local_input_calls: 0,
    };
    let packet = Ipv6Packet::new_checked(&data[..]).unwrap();
    assert!(iface
        .inner
        .process_ipv6_filtered(
            &mut sockets,
            PacketMeta::default(),
            HardwareAddress::Ethernet(advertised_mac),
            &packet,
            &mut scratch,
            &mut filter,
        )
        .is_none());
    assert_eq!(filter.local_input_calls, 1);
    assert!(
        scratch.is_empty(),
        "read-only LOCAL_IN must not copy the packet"
    );
    assert_eq!(
        iface
            .inner
            .neighbor_cache
            .lookup(&src.into(), iface.inner.now),
        NeighborAnswer::Found(HardwareAddress::Ethernet(advertised_mac))
    );

    // An arbitrary ICMPv6 packet may not use NeighborDiscovery to bypass address
    // ownership, even if an integration returns that verdict by mistake.
    data[40] = 128; // Echo request rather than neighbor advertisement.
    Icmpv6Packet::new_unchecked(&mut data[40..]).fill_checksum(&src, &dst);
    let packet = Ipv6Packet::new_checked(&data[..]).unwrap();
    assert!(iface
        .inner
        .process_ipv6_filtered(
            &mut sockets,
            PacketMeta::default(),
            HardwareAddress::Ethernet(advertised_mac),
            &packet,
            &mut scratch,
            &mut filter,
        )
        .is_none());
    assert_eq!(filter.local_input_calls, 1);
}

#[rstest]
#[case::ethernet(Medium::Ethernet)]
#[cfg(feature = "medium-ethernet")]
fn ndisc_neighbor_advertisement_ethernet_multicast_addr(#[case] medium: Medium) {
    let data = [
        0x60, 0x0, 0x0, 0x0, 0x0, 0x20, 0x3a, 0xff, 0xfd, 0xbe, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x2, 0xfd, 0xbe, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x1, 0x88, 0x0, 0x3b, 0xa0, 0x40, 0x0, 0x0, 0x0, 0xfe, 0x80, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x2, 0x2, 0x1, 0xff, 0xff, 0xff,
        0xff, 0xff, 0xff,
    ];

    assert_eq!(
        parse_ipv6(&data),
        Ok(Packet::new_ipv6(
            Ipv6Repr {
                src_addr: Ipv6Address::new(0xfdbe, 0, 0, 0, 0, 0, 0, 0x0002),
                dst_addr: Ipv6Address::new(0xfdbe, 0, 0, 0, 0, 0, 0, 0x0001),
                hop_limit: 255,
                next_header: IpProtocol::Icmpv6,
                payload_len: 32,
            },
            IpPayload::Icmpv6(Icmpv6Repr::Ndisc(NdiscRepr::NeighborAdvert {
                flags: NdiscNeighborFlags::SOLICITED,
                target_addr: Ipv6Address::new(0xfe80, 0, 0, 0, 0, 0, 0, 0x0002),
                lladdr: Some(RawHardwareAddress::from_bytes(&[
                    0xff, 0xff, 0xff, 0xff, 0xff, 0xff
                ])),
            }))
        ))
    );

    let response = None;

    let (mut iface, mut sockets, _device) = setup(medium);

    assert_eq!(
        iface.inner.process_ipv6(
            &mut sockets,
            PacketMeta::default(),
            HardwareAddress::default(),
            &Ipv6Packet::new_checked(&data[..]).unwrap()
        ),
        response
    );

    assert_eq!(
        iface.inner.neighbor_cache.lookup(
            &IpAddress::Ipv6(Ipv6Address::new(0xfdbe, 0, 0, 0, 0, 0, 0, 0x0002)),
            iface.inner.now,
        ),
        NeighborAnswer::NotFound,
    );
}

#[rstest]
#[case::ieee802154(Medium::Ieee802154)]
#[cfg(feature = "medium-ieee802154")]
fn ndisc_neighbor_advertisement_ieee802154(#[case] medium: Medium) {
    let data = [
        0x60, 0x0, 0x0, 0x0, 0x0, 0x28, 0x3a, 0xff, 0xfd, 0xbe, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x2, 0xfd, 0xbe, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x1, 0x88, 0x0, 0x3b, 0x96, 0x40, 0x0, 0x0, 0x0, 0xfe, 0x80, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x2, 0x2, 0x2, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x1, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
    ];

    assert_eq!(
        parse_ipv6(&data),
        Ok(Packet::new_ipv6(
            Ipv6Repr {
                src_addr: Ipv6Address::new(0xfdbe, 0, 0, 0, 0, 0, 0, 0x0002),
                dst_addr: Ipv6Address::new(0xfdbe, 0, 0, 0, 0, 0, 0, 0x0001),
                hop_limit: 255,
                next_header: IpProtocol::Icmpv6,
                payload_len: 40,
            },
            IpPayload::Icmpv6(Icmpv6Repr::Ndisc(NdiscRepr::NeighborAdvert {
                flags: NdiscNeighborFlags::SOLICITED,
                target_addr: Ipv6Address::new(0xfe80, 0, 0, 0, 0, 0, 0, 0x0002),
                lladdr: Some(RawHardwareAddress::from_bytes(&[0, 0, 0, 0, 0, 0, 0, 1])),
            }))
        ))
    );

    let response = None;

    let (mut iface, mut sockets, _device) = setup(medium);

    assert_eq!(
        iface.inner.process_ipv6(
            &mut sockets,
            PacketMeta::default(),
            HardwareAddress::default(),
            &Ipv6Packet::new_checked(&data[..]).unwrap()
        ),
        response
    );

    assert_eq!(
        iface.inner.neighbor_cache.lookup(
            &IpAddress::Ipv6(Ipv6Address::new(0xfdbe, 0, 0, 0, 0, 0, 0, 0x0002)),
            iface.inner.now,
        ),
        NeighborAnswer::Found(HardwareAddress::Ieee802154(Ieee802154Address::from_bytes(
            &[0, 0, 0, 0, 0, 0, 0, 1]
        ))),
    );
}

#[rstest]
#[case(Medium::Ethernet)]
#[cfg(feature = "medium-ethernet")]
fn test_handle_valid_ndisc_request(#[case] medium: Medium) {
    let (mut iface, mut sockets, _device) = setup(medium);

    let mut eth_bytes = vec![0u8; 86];

    let local_ip_addr = Ipv6Address::new(0xfdbe, 0, 0, 0, 0, 0, 0, 1);
    let remote_ip_addr = Ipv6Address::new(0xfdbe, 0, 0, 0, 0, 0, 0, 2);
    let local_hw_addr = EthernetAddress([0x02, 0x02, 0x02, 0x02, 0x02, 0x02]);
    let remote_hw_addr = EthernetAddress([0x52, 0x54, 0x00, 0x00, 0x00, 0x00]);

    let solicit = Icmpv6Repr::Ndisc(NdiscRepr::NeighborSolicit {
        target_addr: local_ip_addr,
        lladdr: Some(remote_hw_addr.into()),
    });
    let ip_repr = IpRepr::Ipv6(Ipv6Repr {
        src_addr: remote_ip_addr,
        dst_addr: local_ip_addr.solicited_node(),
        next_header: IpProtocol::Icmpv6,
        hop_limit: 0xff,
        payload_len: solicit.buffer_len(),
    });

    let mut frame = EthernetFrame::new_unchecked(&mut eth_bytes);
    frame.set_dst_addr(EthernetAddress([0x33, 0x33, 0x00, 0x00, 0x00, 0x00]));
    frame.set_src_addr(remote_hw_addr);
    frame.set_ethertype(EthernetProtocol::Ipv6);
    ip_repr.emit(frame.payload_mut(), &ChecksumCapabilities::default());
    solicit.emit(
        &remote_ip_addr,
        &local_ip_addr.solicited_node(),
        &mut Icmpv6Packet::new_unchecked(&mut frame.payload_mut()[ip_repr.header_len()..]),
        &ChecksumCapabilities::default(),
    );

    iface.set_neighbor_discovery_enabled(false);
    assert_eq!(
        iface.inner.lookup_hardware_addr(
            MockTxToken,
            &IpAddress::Ipv6(remote_ip_addr),
            None,
            &mut iface.fragmenter,
        ),
        Ok((HardwareAddress::Ethernet(local_hw_addr), MockTxToken))
    );
    let icmpv6_expected = Icmpv6Repr::Ndisc(NdiscRepr::NeighborAdvert {
        flags: NdiscNeighborFlags::SOLICITED,
        target_addr: local_ip_addr,
        lladdr: Some(local_hw_addr.into()),
    });
    let ipv6_expected = Ipv6Repr {
        src_addr: local_ip_addr,
        dst_addr: remote_ip_addr,
        next_header: IpProtocol::Icmpv6,
        hop_limit: 0xff,
        payload_len: icmpv6_expected.buffer_len(),
    };
    assert_eq!(
        iface.inner.process_ethernet(
            &mut sockets,
            PacketMeta::default(),
            frame.into_inner(),
            &mut iface.fragments
        ),
        Some(EthernetPacket::Ip(Packet::new_ipv6(
            ipv6_expected,
            IpPayload::Icmpv6(icmpv6_expected)
        )))
    );
    iface.set_neighbor_discovery_enabled(true);
    assert_eq!(
        iface.inner.lookup_hardware_addr(
            MockTxToken,
            &IpAddress::Ipv6(remote_ip_addr),
            None,
            &mut iface.fragmenter,
        ),
        Err(DispatchError::NeighborPending)
    );

    let frame = EthernetFrame::new_unchecked(&mut eth_bytes);

    let icmpv6_expected = Icmpv6Repr::Ndisc(NdiscRepr::NeighborAdvert {
        flags: NdiscNeighborFlags::SOLICITED,
        target_addr: local_ip_addr,
        lladdr: Some(local_hw_addr.into()),
    });

    let ipv6_expected = Ipv6Repr {
        src_addr: local_ip_addr,
        dst_addr: remote_ip_addr,
        next_header: IpProtocol::Icmpv6,
        hop_limit: 0xff,
        payload_len: icmpv6_expected.buffer_len(),
    };

    // Ensure an Neighbor Solicitation triggers a Neighbor Advertisement
    assert_eq!(
        iface.inner.process_ethernet(
            &mut sockets,
            PacketMeta::default(),
            frame.into_inner(),
            &mut iface.fragments
        ),
        Some(EthernetPacket::Ip(Packet::new_ipv6(
            ipv6_expected,
            IpPayload::Icmpv6(icmpv6_expected)
        )))
    );

    // Ensure the address of the requester was entered in the cache
    assert_eq!(
        iface.inner.lookup_hardware_addr(
            MockTxToken,
            &IpAddress::Ipv6(remote_ip_addr),
            None,
            &mut iface.fragmenter,
        ),
        Ok((HardwareAddress::Ethernet(remote_hw_addr), MockTxToken))
    );
}

#[rstest]
#[case::malformed(RawHardwareAddress::from_bytes(&[0x02]))]
#[case::non_unicast(RawHardwareAddress::from_bytes(&[0xff; 6]))]
#[cfg(feature = "medium-ethernet")]
fn disabled_neighbor_discovery_rejects_invalid_ns_source_lladdr(
    #[case] source_lladdr: RawHardwareAddress,
) {
    let (mut iface, _, _) = setup(Medium::Ethernet);
    let local_ip_addr = Ipv6Address::new(0xfdbe, 0, 0, 0, 0, 0, 0, 1);
    let remote_ip_addr = Ipv6Address::new(0xfdbe, 0, 0, 0, 0, 0, 0, 2);
    iface.set_neighbor_discovery_enabled(false);

    assert_eq!(
        iface.inner.process_ndisc(
            Ipv6Repr {
                src_addr: remote_ip_addr,
                dst_addr: local_ip_addr.solicited_node(),
                next_header: IpProtocol::Icmpv6,
                hop_limit: 0xff,
                payload_len: 0,
            },
            NdiscRepr::NeighborSolicit {
                target_addr: local_ip_addr,
                lladdr: Some(source_lladdr),
            },
        ),
        None
    );
}

#[rstest]
#[case(Medium::Ip)]
#[cfg(feature = "medium-ip")]
#[case(Medium::Ethernet)]
#[cfg(feature = "medium-ethernet")]
#[case(Medium::Ieee802154)]
#[cfg(feature = "medium-ieee802154")]
fn test_solicited_node_addrs(#[case] medium: Medium) {
    let (mut iface, _, _) = setup(medium);
    let mut new_addrs = heapless::Vec::<IpCidr, IFACE_MAX_ADDR_COUNT>::new();
    new_addrs
        .push(IpCidr::new(IpAddress::v6(0xfe80, 0, 0, 0, 1, 2, 0, 2), 64))
        .unwrap();
    new_addrs
        .push(IpCidr::new(
            IpAddress::v6(0xfe80, 0, 0, 0, 3, 4, 0, 0xffff),
            64,
        ))
        .unwrap();
    iface.update_ip_addrs(|addrs| {
        new_addrs.extend(addrs.to_vec());
        *addrs = new_addrs;
    });
    assert!(iface
        .inner
        .has_solicited_node(Ipv6Address::new(0xff02, 0, 0, 0, 0, 1, 0xff00, 0x0002)));
    assert!(iface
        .inner
        .has_solicited_node(Ipv6Address::new(0xff02, 0, 0, 0, 0, 1, 0xff00, 0xffff)));
    assert!(!iface
        .inner
        .has_solicited_node(Ipv6Address::new(0xff02, 0, 0, 0, 0, 1, 0xff00, 0x0003)));
}

#[rstest]
#[case(Medium::Ip)]
#[cfg(all(feature = "socket-udp", feature = "medium-ip"))]
#[case(Medium::Ethernet)]
#[cfg(all(feature = "socket-udp", feature = "medium-ethernet"))]
#[case(Medium::Ieee802154)]
#[cfg(all(feature = "socket-udp", feature = "medium-ieee802154"))]
fn test_icmp_reply_size(#[case] medium: Medium) {
    use crate::wire::Icmpv6DstUnreachable;
    use crate::wire::IPV6_MIN_MTU as MIN_MTU;
    const MAX_PAYLOAD_LEN: usize = 1192;

    let (mut iface, mut sockets, _device) = setup(medium);

    let src_addr = Ipv6Address::new(0xfe80, 0, 0, 0, 0, 0, 0, 1);
    let dst_addr = Ipv6Address::new(0xfe80, 0, 0, 0, 0, 0, 0, 2);

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

    let ip_repr = Ipv6Repr {
        src_addr,
        dst_addr,
        next_header: IpProtocol::Udp,
        hop_limit: 64,
        payload_len: udp_repr.header_len() + MAX_PAYLOAD_LEN,
    };
    let payload = packet.into_inner();

    let expected_icmp_repr = Icmpv6Repr::DstUnreachable {
        reason: Icmpv6DstUnreachable::PortUnreachable,
        header: ip_repr,
        data: &payload[..MAX_PAYLOAD_LEN],
    };

    let expected_ip_repr = Ipv6Repr {
        src_addr: dst_addr,
        dst_addr: src_addr,
        next_header: IpProtocol::Icmpv6,
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
        Some(Packet::new_ipv6(
            expected_ip_repr,
            IpPayload::Icmpv6(expected_icmp_repr)
        ))
    );
}

#[cfg(all(feature = "socket-raw", feature = "socket-udp", feature = "medium-ip"))]
#[test]
fn raw_udp_multicast_does_not_generate_port_unreachable() {
    use crate::socket::raw;
    use crate::wire::IpVersion;

    let (mut iface, mut sockets, _) = setup(Medium::Ip);
    let raw_socket = raw::Socket::new(
        IpVersion::Ipv6,
        IpProtocol::Udp,
        raw::PacketBuffer::new(vec![raw::PacketMetadata::EMPTY], vec![0; 128]),
        raw::PacketBuffer::new(vec![raw::PacketMetadata::EMPTY], vec![0; 128]),
    );
    let raw_handle = sockets.add(raw_socket);
    let src_addr = Ipv6Address::new(0xfe80, 0, 0, 0, 0, 0, 0, 1);
    let dst_addr = Ipv6Address::new(0xff02, 0, 0, 0, 0, 0, 0, 1);
    let udp_repr = UdpRepr {
        src_port: 1234,
        dst_port: 5678,
    };
    let mut bytes = vec![0; udp_repr.header_len() + 1];
    udp_repr.emit(
        &mut UdpPacket::new_unchecked(bytes.as_mut_slice()),
        &src_addr.into(),
        &dst_addr.into(),
        1,
        |payload| payload[0] = 42,
        &ChecksumCapabilities::default(),
    );
    let ip_repr = IpRepr::Ipv6(Ipv6Repr {
        src_addr,
        dst_addr,
        next_header: IpProtocol::Udp,
        hop_limit: 1,
        payload_len: bytes.len(),
    });
    assert!(iface
        .inner
        .raw_socket_filter(&mut sockets, &ip_repr, &bytes));
    assert!(sockets.get_mut::<raw::Socket>(raw_handle).can_recv());
    assert_eq!(
        iface
            .inner
            .process_udp(&mut sockets, PacketMeta::default(), ip_repr, &bytes, false),
        None
    );
}

#[cfg(feature = "medium-ip")]
#[test]
fn get_source_address() {
    let (mut iface, _, _) = setup(Medium::Ip);

    const OWN_LINK_LOCAL_ADDR: Ipv6Address = Ipv6Address::new(0xfe80, 0, 0, 0, 0, 0, 0, 1);
    const OWN_UNIQUE_LOCAL_ADDR1: Ipv6Address = Ipv6Address::new(0xfd00, 0, 0, 201, 1, 1, 1, 2);
    const OWN_UNIQUE_LOCAL_ADDR2: Ipv6Address = Ipv6Address::new(0xfd01, 0, 0, 201, 1, 1, 1, 2);
    const OWN_GLOBAL_UNICAST_ADDR1: Ipv6Address =
        Ipv6Address::new(0x2001, 0x0db8, 0x0003, 0, 0, 0, 0, 1);

    // List of addresses of the interface:
    //   fe80::1/64
    //   fd00::201:1:1:1:2/64
    //   fd01::201:1:1:1:2/64
    //   2001:db8:3::1/64
    iface.update_ip_addrs(|addrs| {
        addrs.clear();

        addrs
            .push(IpCidr::Ipv6(Ipv6Cidr::new(OWN_LINK_LOCAL_ADDR, 64)))
            .unwrap();
        addrs
            .push(IpCidr::Ipv6(Ipv6Cidr::new(OWN_UNIQUE_LOCAL_ADDR1, 64)))
            .unwrap();
        addrs
            .push(IpCidr::Ipv6(Ipv6Cidr::new(OWN_UNIQUE_LOCAL_ADDR2, 64)))
            .unwrap();
        addrs
            .push(IpCidr::Ipv6(Ipv6Cidr::new(OWN_GLOBAL_UNICAST_ADDR1, 64)))
            .unwrap();
    });

    // List of addresses we test:
    //   ::1               -> ::1
    //   fe80::42          -> fe80::1
    //   fd00::201:1:1:1:1 -> fd00::201:1:1:1:2
    //   fd01::201:1:1:1:1 -> fd01::201:1:1:1:2
    //   fd02::201:1:1:1:1 -> fd00::201:1:1:1:2 (because first added in the list)
    //   ff02::1           -> fe80::1 (same scope)
    //   2001:db8:3::2     -> 2001:db8:3::1
    //   2001:db9:3::2     -> 2001:db8:3::1
    const LINK_LOCAL_ADDR: Ipv6Address = Ipv6Address::new(0xfe80, 0, 0, 0, 0, 0, 0, 42);
    const UNIQUE_LOCAL_ADDR1: Ipv6Address = Ipv6Address::new(0xfd00, 0, 0, 201, 1, 1, 1, 1);
    const UNIQUE_LOCAL_ADDR2: Ipv6Address = Ipv6Address::new(0xfd01, 0, 0, 201, 1, 1, 1, 1);
    const UNIQUE_LOCAL_ADDR3: Ipv6Address = Ipv6Address::new(0xfd02, 0, 0, 201, 1, 1, 1, 1);
    const GLOBAL_UNICAST_ADDR1: Ipv6Address =
        Ipv6Address::new(0x2001, 0x0db8, 0x0003, 0, 0, 0, 0, 2);
    const GLOBAL_UNICAST_ADDR2: Ipv6Address =
        Ipv6Address::new(0x2001, 0x0db9, 0x0003, 0, 0, 0, 0, 2);

    assert_eq!(
        iface.inner.get_source_address_ipv6(&Ipv6Address::LOCALHOST),
        Ipv6Address::LOCALHOST
    );

    assert_eq!(
        iface.inner.get_source_address_ipv6(&LINK_LOCAL_ADDR),
        OWN_LINK_LOCAL_ADDR
    );
    assert_eq!(
        iface.inner.get_source_address_ipv6(&UNIQUE_LOCAL_ADDR1),
        OWN_UNIQUE_LOCAL_ADDR1
    );
    assert_eq!(
        iface.inner.get_source_address_ipv6(&UNIQUE_LOCAL_ADDR2),
        OWN_UNIQUE_LOCAL_ADDR2
    );
    assert_eq!(
        iface.inner.get_source_address_ipv6(&UNIQUE_LOCAL_ADDR3),
        OWN_UNIQUE_LOCAL_ADDR1
    );
    assert_eq!(
        iface
            .inner
            .get_source_address_ipv6(&IPV6_LINK_LOCAL_ALL_NODES),
        OWN_LINK_LOCAL_ADDR
    );
    assert_eq!(
        iface.inner.get_source_address_ipv6(&GLOBAL_UNICAST_ADDR1),
        OWN_GLOBAL_UNICAST_ADDR1
    );
    assert_eq!(
        iface.inner.get_source_address_ipv6(&GLOBAL_UNICAST_ADDR2),
        OWN_GLOBAL_UNICAST_ADDR1
    );

    assert_eq!(
        iface.get_source_address_ipv6(&LINK_LOCAL_ADDR),
        OWN_LINK_LOCAL_ADDR
    );
    assert_eq!(
        iface.get_source_address_ipv6(&UNIQUE_LOCAL_ADDR1),
        OWN_UNIQUE_LOCAL_ADDR1
    );
    assert_eq!(
        iface.get_source_address_ipv6(&UNIQUE_LOCAL_ADDR2),
        OWN_UNIQUE_LOCAL_ADDR2
    );
    assert_eq!(
        iface.get_source_address_ipv6(&UNIQUE_LOCAL_ADDR3),
        OWN_UNIQUE_LOCAL_ADDR1
    );
    assert_eq!(
        iface.get_source_address_ipv6(&IPV6_LINK_LOCAL_ALL_NODES),
        OWN_LINK_LOCAL_ADDR
    );
    assert_eq!(
        iface.get_source_address_ipv6(&GLOBAL_UNICAST_ADDR1),
        OWN_GLOBAL_UNICAST_ADDR1
    );
    assert_eq!(
        iface.get_source_address_ipv6(&GLOBAL_UNICAST_ADDR2),
        OWN_GLOBAL_UNICAST_ADDR1
    );
}

#[cfg(feature = "medium-ip")]
#[test]
fn get_source_address_only_link_local() {
    let (mut iface, _, _) = setup(Medium::Ip);

    // List of addresses in the interface:
    //   fe80::1/64
    const OWN_LINK_LOCAL_ADDR: Ipv6Address = Ipv6Address::new(0xfe80, 0, 0, 0, 0, 0, 0, 1);
    iface.update_ip_addrs(|ips| {
        ips.clear();
        ips.push(IpCidr::Ipv6(Ipv6Cidr::new(OWN_LINK_LOCAL_ADDR, 64)))
            .unwrap();
    });

    // List of addresses we test:
    //   ::1               -> ::1
    //   fe80::42          -> fe80::1
    //   fd00::201:1:1:1:1 -> fe80::1
    //   fd01::201:1:1:1:1 -> fe80::1
    //   fd02::201:1:1:1:1 -> fe80::1
    //   ff02::1           -> fe80::1
    //   2001:db8:3::2     -> fe80::1
    //   2001:db9:3::2     -> fe80::1
    const LINK_LOCAL_ADDR: Ipv6Address = Ipv6Address::new(0xfe80, 0, 0, 0, 0, 0, 0, 42);
    const UNIQUE_LOCAL_ADDR1: Ipv6Address = Ipv6Address::new(0xfd00, 0, 0, 201, 1, 1, 1, 1);
    const UNIQUE_LOCAL_ADDR2: Ipv6Address = Ipv6Address::new(0xfd01, 0, 0, 201, 1, 1, 1, 1);
    const UNIQUE_LOCAL_ADDR3: Ipv6Address = Ipv6Address::new(0xfd02, 0, 0, 201, 1, 1, 1, 1);
    const GLOBAL_UNICAST_ADDR1: Ipv6Address =
        Ipv6Address::new(0x2001, 0x0db8, 0x0003, 0, 0, 0, 0, 2);
    const GLOBAL_UNICAST_ADDR2: Ipv6Address =
        Ipv6Address::new(0x2001, 0x0db9, 0x0003, 0, 0, 0, 0, 2);

    assert_eq!(
        iface.inner.get_source_address_ipv6(&Ipv6Address::LOCALHOST),
        Ipv6Address::LOCALHOST
    );

    assert_eq!(
        iface.inner.get_source_address_ipv6(&LINK_LOCAL_ADDR),
        OWN_LINK_LOCAL_ADDR
    );
    assert_eq!(
        iface.inner.get_source_address_ipv6(&UNIQUE_LOCAL_ADDR1),
        OWN_LINK_LOCAL_ADDR
    );
    assert_eq!(
        iface.inner.get_source_address_ipv6(&UNIQUE_LOCAL_ADDR2),
        OWN_LINK_LOCAL_ADDR
    );
    assert_eq!(
        iface.inner.get_source_address_ipv6(&UNIQUE_LOCAL_ADDR3),
        OWN_LINK_LOCAL_ADDR
    );
    assert_eq!(
        iface
            .inner
            .get_source_address_ipv6(&IPV6_LINK_LOCAL_ALL_NODES),
        OWN_LINK_LOCAL_ADDR
    );
    assert_eq!(
        iface.inner.get_source_address_ipv6(&GLOBAL_UNICAST_ADDR1),
        OWN_LINK_LOCAL_ADDR
    );
    assert_eq!(
        iface.inner.get_source_address_ipv6(&GLOBAL_UNICAST_ADDR2),
        OWN_LINK_LOCAL_ADDR
    );

    assert_eq!(
        iface.get_source_address_ipv6(&LINK_LOCAL_ADDR),
        OWN_LINK_LOCAL_ADDR
    );
    assert_eq!(
        iface.get_source_address_ipv6(&UNIQUE_LOCAL_ADDR1),
        OWN_LINK_LOCAL_ADDR
    );
    assert_eq!(
        iface.get_source_address_ipv6(&UNIQUE_LOCAL_ADDR2),
        OWN_LINK_LOCAL_ADDR
    );
    assert_eq!(
        iface.get_source_address_ipv6(&UNIQUE_LOCAL_ADDR3),
        OWN_LINK_LOCAL_ADDR
    );
    assert_eq!(
        iface.get_source_address_ipv6(&IPV6_LINK_LOCAL_ALL_NODES),
        OWN_LINK_LOCAL_ADDR
    );
    assert_eq!(
        iface.get_source_address_ipv6(&GLOBAL_UNICAST_ADDR1),
        OWN_LINK_LOCAL_ADDR
    );
    assert_eq!(
        iface.get_source_address_ipv6(&GLOBAL_UNICAST_ADDR2),
        OWN_LINK_LOCAL_ADDR
    );
}

#[cfg(feature = "medium-ip")]
#[test]
fn get_source_address_empty_interface() {
    let (mut iface, _, _) = setup(Medium::Ip);

    iface.update_ip_addrs(|ips| ips.clear());

    // List of addresses we test:
    //   ::1               -> ::1
    //   fe80::42          -> ::1
    //   fd00::201:1:1:1:1 -> ::1
    //   fd01::201:1:1:1:1 -> ::1
    //   fd02::201:1:1:1:1 -> ::1
    //   ff02::1           -> ::1
    //   2001:db8:3::2     -> ::1
    //   2001:db9:3::2     -> ::1
    const LINK_LOCAL_ADDR: Ipv6Address = Ipv6Address::new(0xfe80, 0, 0, 0, 0, 0, 0, 42);
    const UNIQUE_LOCAL_ADDR1: Ipv6Address = Ipv6Address::new(0xfd00, 0, 0, 201, 1, 1, 1, 1);
    const UNIQUE_LOCAL_ADDR2: Ipv6Address = Ipv6Address::new(0xfd01, 0, 0, 201, 1, 1, 1, 1);
    const UNIQUE_LOCAL_ADDR3: Ipv6Address = Ipv6Address::new(0xfd02, 0, 0, 201, 1, 1, 1, 1);
    const GLOBAL_UNICAST_ADDR1: Ipv6Address =
        Ipv6Address::new(0x2001, 0x0db8, 0x0003, 0, 0, 0, 0, 2);
    const GLOBAL_UNICAST_ADDR2: Ipv6Address =
        Ipv6Address::new(0x2001, 0x0db9, 0x0003, 0, 0, 0, 0, 2);

    assert_eq!(
        iface.inner.get_source_address_ipv6(&Ipv6Address::LOCALHOST),
        Ipv6Address::LOCALHOST
    );

    assert_eq!(
        iface.inner.get_source_address_ipv6(&LINK_LOCAL_ADDR),
        Ipv6Address::LOCALHOST
    );
    assert_eq!(
        iface.inner.get_source_address_ipv6(&UNIQUE_LOCAL_ADDR1),
        Ipv6Address::LOCALHOST
    );
    assert_eq!(
        iface.inner.get_source_address_ipv6(&UNIQUE_LOCAL_ADDR2),
        Ipv6Address::LOCALHOST
    );
    assert_eq!(
        iface.inner.get_source_address_ipv6(&UNIQUE_LOCAL_ADDR3),
        Ipv6Address::LOCALHOST
    );
    assert_eq!(
        iface
            .inner
            .get_source_address_ipv6(&IPV6_LINK_LOCAL_ALL_NODES),
        Ipv6Address::LOCALHOST
    );
    assert_eq!(
        iface.inner.get_source_address_ipv6(&GLOBAL_UNICAST_ADDR1),
        Ipv6Address::LOCALHOST
    );
    assert_eq!(
        iface.inner.get_source_address_ipv6(&GLOBAL_UNICAST_ADDR2),
        Ipv6Address::LOCALHOST
    );

    assert_eq!(
        iface.get_source_address_ipv6(&LINK_LOCAL_ADDR),
        Ipv6Address::LOCALHOST
    );
    assert_eq!(
        iface.get_source_address_ipv6(&UNIQUE_LOCAL_ADDR1),
        Ipv6Address::LOCALHOST
    );
    assert_eq!(
        iface.get_source_address_ipv6(&UNIQUE_LOCAL_ADDR2),
        Ipv6Address::LOCALHOST
    );
    assert_eq!(
        iface.get_source_address_ipv6(&UNIQUE_LOCAL_ADDR3),
        Ipv6Address::LOCALHOST
    );
    assert_eq!(
        iface.get_source_address_ipv6(&IPV6_LINK_LOCAL_ALL_NODES),
        Ipv6Address::LOCALHOST
    );
    assert_eq!(
        iface.get_source_address_ipv6(&GLOBAL_UNICAST_ADDR1),
        Ipv6Address::LOCALHOST
    );
    assert_eq!(
        iface.get_source_address_ipv6(&GLOBAL_UNICAST_ADDR2),
        Ipv6Address::LOCALHOST
    );
}

#[rstest]
#[case(Medium::Ip)]
#[cfg(feature = "medium-ip")]
#[case(Medium::Ethernet)]
#[cfg(feature = "medium-ethernet")]
fn test_join_ipv6_multicast_group(#[case] medium: Medium) {
    fn recv_icmpv6(
        device: &mut crate::tests::TestingDevice,
        timestamp: Instant,
    ) -> std::vec::Vec<Ipv6Packet<std::vec::Vec<u8>>> {
        let caps = device.capabilities();
        recv_all(device, timestamp)
            .iter()
            .filter_map(|frame| {
                let ipv6_packet = match caps.medium {
                    #[cfg(feature = "medium-ethernet")]
                    Medium::Ethernet => {
                        let eth_frame = EthernetFrame::new_checked(frame).ok()?;
                        Ipv6Packet::new_checked(eth_frame.payload()).ok()?
                    }
                    #[cfg(feature = "medium-ip")]
                    Medium::Ip => Ipv6Packet::new_checked(&frame[..]).ok()?,
                    #[cfg(feature = "medium-ieee802154")]
                    Medium::Ieee802154 => todo!(),
                };
                let buf = ipv6_packet.into_inner().to_vec();
                Some(Ipv6Packet::new_unchecked(buf))
            })
            .collect::<std::vec::Vec<_>>()
    }

    let (mut iface, mut sockets, mut device) = setup(medium);

    let groups = [
        Ipv6Address::new(0xff05, 0, 0, 0, 0, 0, 0, 0x00fb),
        Ipv6Address::new(0xff0e, 0, 0, 0, 0, 0, 0, 0x0017),
    ];

    let timestamp = Instant::from_millis(0);

    // Drain the unsolicited node multicast report from the device
    iface.poll(timestamp, &mut device, &mut sockets);
    let _ = recv_icmpv6(&mut device, timestamp);

    for &group in &groups {
        iface.join_multicast_group(group).unwrap();
        assert!(iface.has_multicast_group(group));
    }
    assert!(iface.has_multicast_group(IPV6_LINK_LOCAL_ALL_NODES));
    iface.poll(timestamp, &mut device, &mut sockets);
    assert!(iface.has_multicast_group(IPV6_LINK_LOCAL_ALL_NODES));

    let reports = recv_icmpv6(&mut device, timestamp);
    assert_eq!(reports.len(), 2);

    let caps = device.capabilities();
    let checksum_caps = &caps.checksum;
    for (&group_addr, ipv6_packet) in groups.iter().zip(reports) {
        let buf = ipv6_packet.into_inner();
        let ipv6_packet = Ipv6Packet::new_unchecked(buf.as_slice());

        let _ipv6_repr = Ipv6Repr::parse(&ipv6_packet).unwrap();
        let ip_payload = ipv6_packet.payload();

        // The first 2 octets of this payload hold the next-header indicator and the
        // Hop-by-Hop header length (in 8-octet words, minus 1). The remaining 6 octets
        // hold the Hop-by-Hop PadN and Router Alert options.
        let hbh_header = Ipv6HopByHopHeader::new_checked(&ip_payload[..8]).unwrap();
        let hbh_repr = Ipv6HopByHopRepr::parse(&hbh_header).unwrap();

        assert_eq!(hbh_repr.options.len(), 3);
        assert_eq!(
            hbh_repr.options[0],
            Ipv6OptionRepr::Unknown {
                type_: Ipv6OptionType::Unknown(IpProtocol::Icmpv6.into()),
                length: 0,
                data: &[],
            }
        );
        assert_eq!(
            hbh_repr.options[1],
            Ipv6OptionRepr::RouterAlert(Ipv6OptionRouterAlert::MulticastListenerDiscovery)
        );
        assert_eq!(hbh_repr.options[2], Ipv6OptionRepr::PadN(0));

        let icmpv6_packet =
            Icmpv6Packet::new_checked(&ip_payload[hbh_repr.buffer_len()..]).unwrap();
        let icmpv6_repr = Icmpv6Repr::parse(
            &ipv6_packet.src_addr(),
            &ipv6_packet.dst_addr(),
            &icmpv6_packet,
            checksum_caps,
        )
        .unwrap();

        let record_data = match icmpv6_repr {
            Icmpv6Repr::Mld(MldRepr::Report {
                nr_mcast_addr_rcrds,
                data,
            }) => {
                assert_eq!(nr_mcast_addr_rcrds, 1);
                data
            }
            other => panic!("unexpected icmpv6_repr: {:?}", other),
        };

        let record = MldAddressRecord::new_checked(record_data).unwrap();
        let record_repr = MldAddressRecordRepr::parse(&record).unwrap();

        assert_eq!(
            record_repr,
            MldAddressRecordRepr {
                num_srcs: 0,
                mcast_addr: group_addr,
                record_type: MldRecordType::ChangeToExclude,
                aux_data_len: 0,
                payload: &[],
            }
        );

        if !group_addr.is_solicited_node_multicast() {
            iface.leave_multicast_group(group_addr).unwrap();
            assert!(!iface.has_multicast_group(group_addr));
            iface.poll(timestamp, &mut device, &mut sockets);
            assert!(!iface.has_multicast_group(group_addr));
            let leaves = recv_icmpv6(&mut device, timestamp);
            assert_eq!(leaves.len(), 1);
            let bytes = leaves.into_iter().next().unwrap().into_inner();
            let ipv6 = Ipv6Packet::new_unchecked(bytes.as_slice());
            let payload = ipv6.payload();
            let hbh_header = Ipv6HopByHopHeader::new_checked(&payload[..8]).unwrap();
            let hbh = Ipv6HopByHopRepr::parse(&hbh_header).unwrap();
            let packet = Icmpv6Packet::new_checked(&payload[hbh.buffer_len()..]).unwrap();
            let report =
                Icmpv6Repr::parse(&ipv6.src_addr(), &ipv6.dst_addr(), &packet, checksum_caps)
                    .unwrap();
            let Icmpv6Repr::Mld(MldRepr::Report {
                nr_mcast_addr_rcrds,
                data,
            }) = report
            else {
                panic!("expected an MLD leave report");
            };
            assert_eq!(nr_mcast_addr_rcrds, 1);
            let record = MldAddressRecord::new_checked(data).unwrap();
            let record = MldAddressRecordRepr::parse(&record).unwrap();
            assert_eq!(record.mcast_addr, group_addr);
            assert_eq!(record.record_type, MldRecordType::ChangeToInclude);
            assert_eq!(record.num_srcs, 0);
        }
    }
}

#[rstest]
#[case(Medium::Ethernet)]
#[cfg(all(feature = "multicast", feature = "medium-ethernet"))]
fn test_handle_valid_multicast_query(#[case] medium: Medium) {
    fn recv_icmpv6(
        device: &mut crate::tests::TestingDevice,
        timestamp: Instant,
    ) -> std::vec::Vec<Ipv6Packet<std::vec::Vec<u8>>> {
        let caps = device.capabilities();
        recv_all(device, timestamp)
            .iter()
            .filter_map(|frame| {
                let ipv6_packet = match caps.medium {
                    #[cfg(feature = "medium-ethernet")]
                    Medium::Ethernet => {
                        let eth_frame = EthernetFrame::new_checked(frame).ok()?;
                        Ipv6Packet::new_checked(eth_frame.payload()).ok()?
                    }
                    #[cfg(feature = "medium-ip")]
                    Medium::Ip => Ipv6Packet::new_checked(&frame[..]).ok()?,
                    #[cfg(feature = "medium-ieee802154")]
                    Medium::Ieee802154 => todo!(),
                };
                let buf = ipv6_packet.into_inner().to_vec();
                Some(Ipv6Packet::new_unchecked(buf))
            })
            .collect::<std::vec::Vec<_>>()
    }

    let (mut iface, mut sockets, mut device) = setup(medium);

    let mut timestamp = Instant::ZERO;

    let mut eth_bytes = vec![0u8; 86];

    let local_ip_addr = Ipv6Address::new(0xfe80, 0, 0, 0, 0, 0, 0, 1);
    let remote_ip_addr = Ipv6Address::new(0xfe80, 0, 0, 0, 0, 0, 0, 100);
    let remote_hw_addr = EthernetAddress([0x52, 0x54, 0x00, 0x00, 0x00, 0x00]);
    let query_ip_addr = Ipv6Address::new(0xff02, 0, 0, 0, 0, 0, 0, 0x1234);

    iface.join_multicast_group(query_ip_addr).unwrap();

    iface.poll(timestamp, &mut device, &mut sockets);
    // flush multicast reports from the join_multicast_group calls
    recv_icmpv6(&mut device, timestamp);

    let queries = [
        // General query, expect both multicast addresses back
        (
            Ipv6Address::UNSPECIFIED,
            IPV6_LINK_LOCAL_ALL_NODES,
            vec![local_ip_addr.solicited_node(), query_ip_addr],
        ),
        // Address specific query, expect only the queried address back
        (query_ip_addr, query_ip_addr, vec![query_ip_addr]),
    ];

    for (mcast_query, address, _results) in queries.iter() {
        let query = Icmpv6Repr::Mld(MldRepr::Query {
            max_resp_code: 1000,
            mcast_addr: *mcast_query,
            s_flag: false,
            qrv: 1,
            qqic: 60,
            num_srcs: 0,
            data: &[0, 0, 0, 0],
        });

        let ip_repr = IpRepr::Ipv6(Ipv6Repr {
            src_addr: remote_ip_addr,
            dst_addr: *address,
            next_header: IpProtocol::Icmpv6,
            hop_limit: 1,
            payload_len: query.buffer_len(),
        });

        let mut frame = EthernetFrame::new_unchecked(&mut eth_bytes);
        frame.set_dst_addr(EthernetAddress([0x33, 0x33, 0x00, 0x00, 0x00, 0x00]));
        frame.set_src_addr(remote_hw_addr);
        frame.set_ethertype(EthernetProtocol::Ipv6);
        ip_repr.emit(frame.payload_mut(), &ChecksumCapabilities::default());
        query.emit(
            &remote_ip_addr,
            address,
            &mut Icmpv6Packet::new_unchecked(&mut frame.payload_mut()[ip_repr.header_len()..]),
            &ChecksumCapabilities::default(),
        );

        iface.inner.process_ethernet(
            &mut sockets,
            PacketMeta::default(),
            frame.into_inner(),
            &mut iface.fragments,
        );

        timestamp += crate::time::Duration::from_millis(1000);
        iface.poll(timestamp, &mut device, &mut sockets);
    }

    let reports = recv_icmpv6(&mut device, timestamp);
    assert_eq!(reports.len(), queries.len());

    let caps = device.capabilities();
    let checksum_caps = &caps.checksum;
    for ((_mcast_query, _address, results), ipv6_packet) in queries.iter().zip(reports) {
        let buf = ipv6_packet.into_inner();
        let ipv6_packet = Ipv6Packet::new_unchecked(buf.as_slice());

        let ipv6_repr = Ipv6Repr::parse(&ipv6_packet).unwrap();
        let ip_payload = ipv6_packet.payload();
        assert_eq!(ipv6_repr.dst_addr, IPV6_LINK_LOCAL_ALL_MLDV2_ROUTERS);

        // The first 2 octets of this payload hold the next-header indicator and the
        // Hop-by-Hop header length (in 8-octet words, minus 1). The remaining 6 octets
        // hold the Hop-by-Hop PadN and Router Alert options.
        let hbh_header = Ipv6HopByHopHeader::new_checked(&ip_payload[..8]).unwrap();
        let hbh_repr = Ipv6HopByHopRepr::parse(&hbh_header).unwrap();

        assert_eq!(hbh_repr.options.len(), 3);
        assert_eq!(
            hbh_repr.options[0],
            Ipv6OptionRepr::Unknown {
                type_: Ipv6OptionType::Unknown(IpProtocol::Icmpv6.into()),
                length: 0,
                data: &[],
            }
        );
        assert_eq!(
            hbh_repr.options[1],
            Ipv6OptionRepr::RouterAlert(Ipv6OptionRouterAlert::MulticastListenerDiscovery)
        );
        assert_eq!(hbh_repr.options[2], Ipv6OptionRepr::PadN(0));

        let icmpv6_packet =
            Icmpv6Packet::new_checked(&ip_payload[hbh_repr.buffer_len()..]).unwrap();
        let icmpv6_repr = Icmpv6Repr::parse(
            &ipv6_packet.src_addr(),
            &ipv6_packet.dst_addr(),
            &icmpv6_packet,
            checksum_caps,
        )
        .unwrap();

        let record_data = match icmpv6_repr {
            Icmpv6Repr::Mld(MldRepr::Report {
                nr_mcast_addr_rcrds,
                data,
            }) => {
                assert_eq!(nr_mcast_addr_rcrds, results.len() as u16);
                data
            }
            other => panic!("unexpected icmpv6_repr: {:?}", other),
        };

        let mut record_reprs = Vec::new();
        let mut payload = record_data;

        // FIXME: parsing multiple address records should be done by the MLD code
        while !payload.is_empty() {
            let record = MldAddressRecord::new_checked(payload).unwrap();
            let mut record_repr = MldAddressRecordRepr::parse(&record).unwrap();
            payload = record_repr.payload;
            record_repr.payload = &[];
            record_reprs.push(record_repr);
        }

        let expected_records = results
            .iter()
            .map(|addr| MldAddressRecordRepr {
                num_srcs: 0,
                mcast_addr: *addr,
                record_type: MldRecordType::ModeIsExclude,
                aux_data_len: 0,
                payload: &[],
            })
            .collect::<Vec<_>>();

        assert_eq!(record_reprs, expected_records);
    }
}

#[rstest]
#[case(Medium::Ethernet)]
#[cfg(all(feature = "multicast", feature = "medium-ethernet"))]
fn test_solicited_node_multicast_autojoin(#[case] medium: Medium) {
    let (mut iface, _, _) = setup(medium);

    let addr1 = Ipv6Address::new(0xfe80, 0, 0, 0, 0, 0, 0, 1);
    let addr2 = Ipv6Address::new(0xfe80, 0, 0, 0, 0, 0, 0, 2);

    iface.update_ip_addrs(|ip_addrs| {
        ip_addrs.clear();
        ip_addrs.push(IpCidr::new(addr1.into(), 64)).unwrap();
    });
    assert!(iface.has_multicast_group(addr1.solicited_node()));
    assert!(!iface.has_multicast_group(addr2.solicited_node()));

    iface.update_ip_addrs(|ip_addrs| {
        ip_addrs.clear();
        ip_addrs.push(IpCidr::new(addr2.into(), 64)).unwrap();
    });
    assert!(!iface.has_multicast_group(addr1.solicited_node()));
    assert!(iface.has_multicast_group(addr2.solicited_node()));

    iface.update_ip_addrs(|ip_addrs| {
        ip_addrs.clear();
        ip_addrs.push(IpCidr::new(addr1.into(), 64)).unwrap();
        ip_addrs.push(IpCidr::new(addr2.into(), 64)).unwrap();
    });
    assert!(iface.has_multicast_group(addr1.solicited_node()));
    assert!(iface.has_multicast_group(addr2.solicited_node()));

    iface.update_ip_addrs(|ip_addrs| {
        ip_addrs.clear();
    });
    assert!(!iface.has_multicast_group(addr1.solicited_node()));
    assert!(!iface.has_multicast_group(addr2.solicited_node()));
}
