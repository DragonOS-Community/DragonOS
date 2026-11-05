use super::*;
use crate::iface::TcpListenRegistry;
use crate::socket::tcp;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicUsize, Ordering};

#[derive(Debug)]
struct Registry {
    endpoint: IpEndpoint,
    calls: AtomicUsize,
}

impl TcpListenRegistry for Registry {
    fn is_listening(&self, endpoint: IpEndpoint, _meta: PacketMeta) -> bool {
        self.calls.fetch_add(1, Ordering::Relaxed);
        endpoint == self.endpoint
    }
}

fn syn() -> TcpRepr<'static> {
    TcpRepr {
        src_port: 4242,
        dst_port: 4243,
        control: TcpControl::Syn,
        seq_number: TcpSeqNumber(100),
        ack_number: None,
        urgent_at: None,
        window_len: 256,
        window_scale: None,
        max_seg_size: None,
        sack_permitted: false,
        sack_ranges: [None; 3],
        timestamp: None,
        payload: &[],
    }
}

fn emit(ip: &IpRepr, tcp: &TcpRepr) -> Vec<u8> {
    let mut bytes = vec![0; tcp.buffer_len()];
    tcp.emit(
        &mut TcpPacket::new_unchecked(&mut bytes),
        &ip.src_addr(),
        &ip.dst_addr(),
        &ChecksumCapabilities::default(),
    );
    bytes
}

#[test]
#[cfg(feature = "proto-ipv4")]
fn listener_fallback_ipv4() {
    listener_fallback(IpRepr::Ipv4(Ipv4Repr {
        src_addr: Ipv4Address::new(192, 168, 1, 2),
        dst_addr: Ipv4Address::new(192, 168, 1, 1),
        next_header: IpProtocol::Tcp,
        payload_len: 20,
        hop_limit: 64,
    }));
}

#[test]
#[cfg(feature = "proto-ipv4")]
fn tcp_same_tuple_loopback_connect() {
    let (mut iface, mut sockets, _) = setup(Medium::Ip);
    let mut device = crate::phy::Loopback::new(Medium::Ip);
    let endpoint = IpEndpoint::new(Ipv4Address::new(127, 0, 0, 1).into(), 4243);
    let mut socket = tcp::Socket::new(
        tcp::SocketBuffer::new(vec![0; 128]),
        tcp::SocketBuffer::new(vec![0; 128]),
    );
    socket
        .connect(&mut iface.inner, endpoint, endpoint)
        .unwrap();
    let handle = sockets.add(socket);
    for tick in 0..16 {
        iface.inner.now = Instant::from_millis(tick * 100);
        iface.socket_egress(&mut device, &mut sockets);
        iface.poll_ingress_single(iface.inner.now, &mut device, &mut sockets);
        if sockets.get::<tcp::Socket>(handle).state() == tcp::State::Established {
            break;
        }
    }
    assert_eq!(
        sockets.get::<tcp::Socket>(handle).state(),
        tcp::State::Established
    );
    sockets
        .get_mut::<tcp::Socket>(handle)
        .send_slice(b"self")
        .unwrap();
    for tick in 16..32 {
        iface.inner.now = Instant::from_millis(tick * 100);
        iface.socket_egress(&mut device, &mut sockets);
        iface.poll_ingress_single(iface.inner.now, &mut device, &mut sockets);
        if sockets.get::<tcp::Socket>(handle).recv_queue() != 0 {
            break;
        }
    }
    let mut bytes = [0; 4];
    assert_eq!(
        sockets
            .get_mut::<tcp::Socket>(handle)
            .recv_slice(&mut bytes),
        Ok(4)
    );
    assert_eq!(&bytes, b"self");
}
#[test]
#[cfg(feature = "proto-ipv6")]
fn listener_fallback_ipv6() {
    listener_fallback(IpRepr::Ipv6(Ipv6Repr {
        src_addr: Ipv6Address::new(0xfe80, 0, 0, 0, 0, 0, 0, 2),
        dst_addr: Ipv6Address::new(0xfe80, 0, 0, 0, 0, 0, 0, 1),
        next_header: IpProtocol::Tcp,
        payload_len: 20,
        hop_limit: 64,
    }));
}

fn listener_fallback(ip: IpRepr) {
    let (mut iface, mut sockets, _) = setup(Medium::Ip);
    let mut tcp = syn();
    let registry = Arc::new(Registry {
        endpoint: IpEndpoint::new(ip.dst_addr(), tcp.dst_port),
        calls: AtomicUsize::new(0),
    });
    let bytes = emit(&ip, &tcp);
    assert!(iface
        .inner
        .process_tcp(&mut sockets, PacketMeta::default(), ip.clone(), &bytes)
        .is_some());
    assert!(sockets
        .set_tcp_listen_registry(Some(registry.clone()))
        .is_none());
    assert!(iface
        .inner
        .process_tcp(&mut sockets, PacketMeta::default(), ip.clone(), &bytes)
        .is_none());
    assert_eq!(registry.calls.load(Ordering::Relaxed), 1);

    // A different port or destination address is not covered by the registry.
    tcp.dst_port += 1;
    assert!(iface
        .inner
        .process_tcp(
            &mut sockets,
            PacketMeta::default(),
            ip.clone(),
            &emit(&ip, &tcp)
        )
        .is_some());
    tcp.dst_port -= 1;
    let other_ip = match ip.clone() {
        #[cfg(feature = "proto-ipv4")]
        IpRepr::Ipv4(mut repr) => {
            repr.dst_addr = repr.src_addr;
            IpRepr::Ipv4(repr)
        }
        #[cfg(feature = "proto-ipv6")]
        IpRepr::Ipv6(mut repr) => {
            repr.dst_addr = repr.src_addr;
            IpRepr::Ipv6(repr)
        }
    };
    assert!(iface
        .inner
        .process_tcp(
            &mut sockets,
            PacketMeta::default(),
            other_ip.clone(),
            &emit(&other_ip, &tcp)
        )
        .is_some());
    let calls = registry.calls.load(Ordering::Relaxed);
    let mut corrupt = bytes.clone();
    corrupt[16] ^= 1;
    assert!(iface
        .inner
        .process_tcp(&mut sockets, PacketMeta::default(), ip.clone(), &corrupt)
        .is_none());
    assert!(iface
        .inner
        .process_tcp(
            &mut sockets,
            PacketMeta::default(),
            ip.clone(),
            &bytes[..10]
        )
        .is_none());
    // SYN+ACK and ordinary ACK retain the closed-port reset; RST gets no reply.
    tcp.ack_number = Some(TcpSeqNumber(1));
    assert!(iface
        .inner
        .process_tcp(
            &mut sockets,
            PacketMeta::default(),
            ip.clone(),
            &emit(&ip, &tcp)
        )
        .is_some());
    tcp.control = TcpControl::None;
    assert!(iface
        .inner
        .process_tcp(
            &mut sockets,
            PacketMeta::default(),
            ip.clone(),
            &emit(&ip, &tcp)
        )
        .is_some());
    tcp.control = TcpControl::Rst;
    assert!(iface
        .inner
        .process_tcp(
            &mut sockets,
            PacketMeta::default(),
            ip.clone(),
            &emit(&ip, &tcp)
        )
        .is_none());
    assert_eq!(registry.calls.load(Ordering::Relaxed), calls);
    assert!(sockets.set_tcp_listen_registry(None).is_some());
    assert!(iface
        .inner
        .process_tcp(&mut sockets, PacketMeta::default(), ip.clone(), &bytes)
        .is_some());
    assert_eq!(registry.calls.load(Ordering::Relaxed), calls);

    // A real listening socket, its SYN retransmission, and established data
    // take precedence over the registry, even though it matches the endpoint.
    sockets.set_tcp_listen_registry(Some(registry.clone()));
    let mut socket = tcp::Socket::new(
        tcp::SocketBuffer::new(vec![0; 128]),
        tcp::SocketBuffer::new(vec![0; 128]),
    );
    socket.listen(registry.endpoint).unwrap();
    let handle = sockets.add(socket);
    assert!(iface
        .inner
        .process_tcp(&mut sockets, PacketMeta::default(), ip.clone(), &bytes)
        .is_none());
    let mut server_seq = None;
    sockets
        .get_mut::<tcp::Socket>(handle)
        .dispatch(&mut iface.inner, |_, (_, repr)| {
            assert_eq!(repr.control, TcpControl::Syn);
            server_seq = Some(repr.seq_number);
            Ok::<(), ()>(())
        })
        .unwrap();
    let server_seq = server_seq.unwrap();
    assert_eq!(
        sockets.get::<tcp::Socket>(handle).state(),
        tcp::State::SynReceived
    );
    iface
        .inner
        .process_tcp(&mut sockets, PacketMeta::default(), ip.clone(), &bytes);
    tcp = syn();
    tcp.control = TcpControl::None;
    tcp.seq_number += 1;
    tcp.ack_number = Some(server_seq + 1);
    iface.inner.process_tcp(
        &mut sockets,
        PacketMeta::default(),
        ip.clone(),
        &emit(&ip, &tcp),
    );
    assert_eq!(
        sockets.get::<tcp::Socket>(handle).state(),
        tcp::State::Established
    );
    tcp.payload = b"hello";
    iface.inner.process_tcp(
        &mut sockets,
        PacketMeta::default(),
        ip.clone(),
        &emit(&ip, &tcp),
    );
    let mut received = [0; 5];
    assert_eq!(
        sockets
            .get_mut::<tcp::Socket>(handle)
            .recv_slice(&mut received),
        Ok(5)
    );
    assert_eq!(&received, b"hello");
    assert_eq!(registry.calls.load(Ordering::Relaxed), calls);
}

#[cfg(all(feature = "packetmeta-id", feature = "proto-ipv4"))]
mod device_binding {
    use super::*;
    use core::num::NonZeroU32;

    struct CaptureDevice {
        inner: crate::tests::TestingDevice,
        sent: Vec<u32>,
        ingress: u32,
    }

    struct CaptureTx<'a> {
        inner: crate::tests::TxToken<'a>,
        sent: &'a mut Vec<u32>,
        meta: PacketMeta,
    }

    impl TxToken for CaptureTx<'_> {
        fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
            self.sent.push(self.meta.id);
            self.inner.consume(len, f)
        }

        fn set_meta(&mut self, meta: PacketMeta) {
            self.meta = meta;
        }
    }

    struct CaptureRx {
        inner: crate::tests::RxToken,
        id: u32,
    }

    impl RxToken for CaptureRx {
        fn consume<R, F: FnOnce(&[u8]) -> R>(self, f: F) -> R {
            self.inner.consume(f)
        }

        fn meta(&self) -> PacketMeta {
            PacketMeta { id: self.id }
        }
    }

    impl Device for CaptureDevice {
        type RxToken<'a> = CaptureRx;
        type TxToken<'a> = CaptureTx<'a>;

        fn capabilities(&self) -> DeviceCapabilities {
            self.inner.capabilities()
        }

        fn receive(&mut self, now: Instant) -> Option<(CaptureRx, CaptureTx<'_>)> {
            let (rx, tx) = self.inner.receive(now)?;
            Some((
                CaptureRx {
                    inner: rx,
                    id: self.ingress,
                },
                CaptureTx {
                    inner: tx,
                    sent: &mut self.sent,
                    meta: PacketMeta::default(),
                },
            ))
        }

        fn transmit(&mut self, now: Instant) -> Option<CaptureTx<'_>> {
            Some(CaptureTx {
                inner: self.inner.transmit(now)?,
                sent: &mut self.sent,
                meta: PacketMeta::default(),
            })
        }
    }

    fn ip() -> IpRepr {
        IpRepr::Ipv4(Ipv4Repr {
            src_addr: Ipv4Address::new(192, 168, 1, 2),
            dst_addr: Ipv4Address::new(192, 168, 1, 1),
            next_header: IpProtocol::Tcp,
            payload_len: 20,
            hop_limit: 64,
        })
    }

    fn frame(medium: Medium, ip: &IpRepr, tcp: &TcpRepr) -> Vec<u8> {
        let offset = match medium {
            Medium::Ip => 0,
            #[cfg(feature = "medium-ethernet")]
            Medium::Ethernet => 14,
            #[allow(unreachable_patterns)]
            _ => unreachable!(),
        };
        let mut bytes = vec![0; offset + ip.header_len() + tcp.buffer_len()];
        #[cfg(feature = "medium-ethernet")]
        if medium == Medium::Ethernet {
            let mut eth = EthernetFrame::new_unchecked(&mut bytes[..]);
            eth.set_src_addr(EthernetAddress([2, 2, 2, 2, 2, 3]));
            eth.set_dst_addr(EthernetAddress([2, 2, 2, 2, 2, 2]));
            eth.set_ethertype(EthernetProtocol::Ipv4);
        }
        let IpRepr::Ipv4(mut repr) = ip.clone() else {
            unreachable!()
        };
        repr.payload_len = tcp.buffer_len();
        repr.emit(
            &mut Ipv4Packet::new_unchecked(&mut bytes[offset..]),
            &ChecksumCapabilities::default(),
        );
        bytes[offset + ip.header_len()..].copy_from_slice(&emit(ip, tcp));
        bytes
    }

    #[rstest]
    #[case::ip(Medium::Ip)]
    #[cfg(feature = "medium-ethernet")]
    #[case::ethernet(Medium::Ethernet)]
    fn tcp_device_binding_input_and_both_output_paths(#[case] medium: Medium) {
        for bound in [None, NonZeroU32::new(7)] {
            let (mut iface, mut sockets, inner) = setup(medium);
            let mut device = CaptureDevice {
                inner,
                sent: vec![],
                ingress: 7,
            };
            let ip = ip();
            #[cfg(feature = "medium-ethernet")]
            if medium == Medium::Ethernet {
                iface.inner.neighbor_cache.fill(
                    ip.src_addr(),
                    HardwareAddress::Ethernet(EthernetAddress([2, 2, 2, 2, 2, 3])),
                    Instant::ZERO,
                );
            }
            let mut socket = tcp::Socket::new(
                tcp::SocketBuffer::new(vec![0; 128]),
                tcp::SocketBuffer::new(vec![0; 128]),
            );
            socket.set_bound_device(bound);
            socket.listen(4243).unwrap();
            let handle = sockets.add(socket);

            // Wrong/unknown ingress must not start a bound listener's handshake.
            if bound.is_some() {
                for id in [0, 8] {
                    iface.inner.process_tcp(
                        &mut sockets,
                        PacketMeta { id },
                        ip.clone(),
                        &emit(&ip, &syn()),
                    );
                    assert_eq!(
                        sockets.get::<tcp::Socket>(handle).state(),
                        tcp::State::Listen
                    );
                }
            }
            device.inner.rx_queue.push_back(frame(medium, &ip, &syn()));
            iface.poll_ingress_single(Instant::ZERO, &mut device, &mut sockets);
            assert_eq!(
                sockets.get::<tcp::Socket>(handle).state(),
                tcp::State::SynReceived
            );
            iface.socket_egress(&mut device, &mut sockets);
            let expected = bound.map_or(0, NonZeroU32::get);
            assert_eq!(device.sent, vec![expected]);
            let bytes = device.inner.tx_queue.pop_front().unwrap();
            let offset = if medium == Medium::Ip { 0 } else { 14 };
            let ip_packet = Ipv4Packet::new_checked(&bytes[offset..]).unwrap();
            let reply = TcpPacket::new_checked(ip_packet.payload()).unwrap();
            let mut ack = syn();
            ack.control = TcpControl::None;
            ack.seq_number += 1;
            ack.ack_number = Some(reply.seq_number() + 1);
            // Changing the listener configuration preserves the pending child.
            sockets
                .get_mut::<tcp::Socket>(handle)
                .set_listen_bound_device(NonZeroU32::new(8));
            if bound.is_some() {
                iface.inner.process_tcp(
                    &mut sockets,
                    PacketMeta { id: 8 },
                    ip.clone(),
                    &emit(&ip, &ack),
                );
                assert_eq!(
                    sockets.get::<tcp::Socket>(handle).state(),
                    tcp::State::SynReceived
                );
            }
            device.inner.rx_queue.push_back(frame(medium, &ip, &ack));
            iface.poll_ingress_single(Instant::ZERO, &mut device, &mut sockets);
            assert_eq!(
                sockets.get::<tcp::Socket>(handle).state(),
                tcp::State::Established
            );
            assert_eq!(sockets.get::<tcp::Socket>(handle).bound_device(), bound);

            // An out-of-window segment elicits an immediate ACK. Observe the
            // actual TX token, including the Ethernet response dispatch path.
            ack.seq_number += 1000;
            ack.payload = b"x";
            device.inner.rx_queue.push_back(frame(medium, &ip, &ack));
            iface.poll_ingress_single(Instant::ZERO, &mut device, &mut sockets);
            assert_eq!(device.sent, vec![expected, expected]);
            assert_eq!(sockets.get::<tcp::Socket>(handle).recv_queue(), 0);
        }
    }

    #[test]
    fn tcp_device_binding_overflow_registry() {
        #[derive(Debug)]
        struct BoundRegistry;
        impl TcpListenRegistry for BoundRegistry {
            fn is_listening(&self, endpoint: IpEndpoint, meta: PacketMeta) -> bool {
                endpoint == IpEndpoint::new(ip().dst_addr(), 4243) && meta.id == 7
            }
        }
        let (mut iface, mut sockets, _) = setup(Medium::Ip);
        sockets.set_tcp_listen_registry(Some(Arc::new(BoundRegistry)));
        let ip = ip();
        let bytes = emit(&ip, &syn());
        assert!(iface
            .inner
            .process_tcp(&mut sockets, PacketMeta { id: 7 }, ip.clone(), &bytes)
            .is_none());
        let reset = iface
            .inner
            .process_tcp(&mut sockets, PacketMeta { id: 8 }, ip.clone(), &bytes)
            .unwrap();
        assert_eq!(reset.tx_meta().id, 0);
    }

    #[test]
    #[cfg(feature = "proto-ipv6")]
    fn tcp_device_binding_ipv6_dispatch_metadata() {
        let (mut iface, _, inner) = setup(Medium::Ip);
        let mut device = CaptureDevice {
            inner,
            sent: vec![],
            ingress: 0,
        };
        let packet = Packet::new_ipv6(
            Ipv6Repr {
                src_addr: Ipv6Address::LOCALHOST,
                dst_addr: Ipv6Address::LOCALHOST,
                next_header: IpProtocol::Tcp,
                payload_len: syn().buffer_len(),
                hop_limit: 64,
            },
            IpPayload::Tcp(syn()),
        )
        .with_tx_meta(PacketMeta { id: 7 });
        iface
            .inner
            .dispatch_ip(
                device.transmit(Instant::ZERO).unwrap(),
                packet.tx_meta(),
                packet,
                &mut iface.fragmenter,
            )
            .unwrap();
        assert_eq!(device.sent, vec![7]);
    }
}
