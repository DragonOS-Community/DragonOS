use super::*;
use crate::iface::{TcpIngressHandler, TcpIngressResult};
use crate::socket::tcp;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicUsize, Ordering};

#[derive(Debug)]
struct Handler {
    result: TcpIngressResult,
    calls: AtomicUsize,
}

impl TcpIngressHandler for Handler {
    fn handle_tcp_ingress(
        &self,
        _meta: PacketMeta,
        ip: &IpRepr,
        segment: &[u8],
    ) -> TcpIngressResult {
        assert_eq!(ip.next_header(), IpProtocol::Tcp);
        assert_eq!(ip.payload_len(), segment.len());
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.result
    }
}

struct TransportDevice {
    inner: crate::tests::TestingDevice,
    available: bool,
}

impl Device for TransportDevice {
    type RxToken<'a> = crate::tests::RxToken;
    type TxToken<'a> = crate::tests::TxToken<'a>;

    fn capabilities(&self) -> crate::phy::DeviceCapabilities {
        self.inner.capabilities()
    }

    fn receive(&mut self, _: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        None
    }

    fn transmit(&mut self, now: Instant) -> Option<Self::TxToken<'_>> {
        if self.available {
            self.inner.transmit(now)
        } else {
            None
        }
    }

    fn outbound_ip_mtu(&self, _: IpAddress, _: PacketMeta) -> usize {
        1280
    }
}

fn segment(ip: &mut IpRepr) -> Vec<u8> {
    let repr = TcpRepr {
        src_port: 40000,
        dst_port: 80,
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
    };
    ip.set_payload_len(repr.buffer_len());
    let mut bytes = vec![0; repr.buffer_len()];
    repr.emit(
        &mut TcpPacket::new_unchecked(&mut bytes),
        &ip.src_addr(),
        &ip.dst_addr(),
        &ChecksumCapabilities::default(),
    );
    bytes
}

fn addresses() -> Vec<IpRepr> {
    vec![
        #[cfg(feature = "proto-ipv4")]
        IpRepr::Ipv4(Ipv4Repr {
            src_addr: Ipv4Address::new(192, 0, 2, 1),
            dst_addr: Ipv4Address::new(192, 0, 2, 2),
            next_header: IpProtocol::Tcp,
            payload_len: 0,
            hop_limit: 64,
        }),
        #[cfg(feature = "proto-ipv6")]
        IpRepr::Ipv6(Ipv6Repr {
            src_addr: Ipv6Address::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1),
            dst_addr: Ipv6Address::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 2),
            next_header: IpProtocol::Tcp,
            payload_len: 0,
            hop_limit: 64,
        }),
    ]
}

#[test]
fn tcp_handler_consumed_suppresses_socket_and_reset() {
    for mut ip in addresses() {
        let (mut iface, mut sockets, _) = setup(Medium::Ip);
        let handler = Arc::new(Handler {
            result: TcpIngressResult::Consumed,
            calls: AtomicUsize::new(0),
        });
        sockets.set_tcp_ingress_handler(Some(handler.clone()));
        let mut listener = tcp::Socket::new(
            tcp::SocketBuffer::new(vec![0; 64]),
            tcp::SocketBuffer::new(vec![0; 64]),
        );
        listener.listen(80).unwrap();
        let handle = sockets.add(listener);
        let bytes = segment(&mut ip);
        assert!(iface
            .inner
            .process_tcp(&mut sockets, PacketMeta::default(), ip.clone(), &bytes)
            .is_none());
        assert_eq!(handler.calls.load(Ordering::Relaxed), 1);
        assert_eq!(
            sockets.get::<tcp::Socket>(handle).state(),
            tcp::State::Listen
        );
        sockets.remove(handle);
        assert!(iface
            .inner
            .process_tcp(&mut sockets, PacketMeta::default(), ip, &bytes)
            .is_none());
        assert_eq!(handler.calls.load(Ordering::Relaxed), 2);
    }
}

#[test]
#[cfg(feature = "proto-ipv6")]
fn tcp_handler_normalizes_ipv6_extension_metadata() {
    let (mut iface, mut sockets, _) = setup(Medium::Ip);
    let handler = Arc::new(Handler {
        result: TcpIngressResult::Consumed,
        calls: AtomicUsize::new(0),
    });
    sockets.set_tcp_ingress_handler(Some(handler.clone()));
    let mut ip = addresses()
        .into_iter()
        .find(|ip| matches!(ip, IpRepr::Ipv6(_)))
        .unwrap();
    let bytes = segment(&mut ip);
    let IpRepr::Ipv6(mut ipv6) = ip else {
        unreachable!()
    };
    ipv6.next_header = IpProtocol::HopByHop;
    ipv6.payload_len += 8;
    assert!(iface
        .inner
        .process_tcp(&mut sockets, PacketMeta::default(), ipv6.into(), &bytes,)
        .is_none());
    assert_eq!(handler.calls.load(Ordering::Relaxed), 1);
}

#[test]
#[cfg(feature = "packetmeta-id")]
fn tcp_transport_preserves_device_constraint() {
    for mut ip in addresses() {
        let (mut iface, mut sockets, mut device) = setup(Medium::Ip);
        let mut socket = tcp::Socket::new(
            tcp::SocketBuffer::new(vec![0; 256]),
            tcp::SocketBuffer::new(vec![0; 256]),
        );
        socket.listen(80).unwrap();
        socket.set_bound_device(core::num::NonZeroU32::new(7));
        let handle = sockets.add(socket);
        let bytes = segment(&mut ip);
        let mut meta = PacketMeta { id: 8 };
        assert!(iface.process_tcp_ingress(
            Instant::ZERO,
            &mut device,
            &mut sockets,
            meta,
            ip.clone(),
            &bytes
        ));
        assert_eq!(
            sockets.get::<tcp::Socket>(handle).state(),
            tcp::State::Listen
        );
        meta.id = 7;
        assert!(iface.process_tcp_ingress(
            Instant::ZERO,
            &mut device,
            &mut sockets,
            meta,
            ip,
            &bytes
        ));
        assert_eq!(
            sockets.get::<tcp::Socket>(handle).state(),
            tcp::State::SynReceived
        );
    }
}

#[test]
fn tcp_handler_not_handled_preserves_reset_and_bad_checksum_is_not_offered() {
    for mut ip in addresses() {
        let (mut iface, mut sockets, _) = setup(Medium::Ip);
        let handler = Arc::new(Handler {
            result: TcpIngressResult::NotHandled,
            calls: AtomicUsize::new(0),
        });
        sockets.set_tcp_ingress_handler(Some(handler.clone()));
        let mut bytes = segment(&mut ip);
        assert!(iface
            .inner
            .process_tcp(&mut sockets, PacketMeta::default(), ip.clone(), &bytes)
            .is_some());
        bytes[16] ^= 1;
        assert!(iface
            .inner
            .process_tcp(&mut sockets, PacketMeta::default(), ip, &bytes)
            .is_none());
        assert_eq!(handler.calls.load(Ordering::Relaxed), 1);
    }
}

#[test]
fn tcp_transport_unconfigured_destination_capacity_and_routed_synack_mss() {
    for mut ip in addresses() {
        let (mut iface, mut sockets, _) = setup(Medium::Ip);
        iface.update_ip_addrs(|addrs| addrs.clear());
        let mut socket = tcp::Socket::new(
            tcp::SocketBuffer::new(vec![0; 256]),
            tcp::SocketBuffer::new(vec![0; 256]),
        );
        socket.listen(IpListenEndpoint::from(80)).unwrap();
        let handle = sockets.add(socket);
        let mut device = TransportDevice {
            inner: crate::tests::TestingDevice::new(Medium::Ip),
            available: false,
        };
        let bytes = segment(&mut ip);
        assert!(!iface.process_tcp_ingress(
            Instant::from_millis(123),
            &mut device,
            &mut sockets,
            PacketMeta::default(),
            ip.clone(),
            &bytes,
        ));
        assert_eq!(
            sockets.get::<tcp::Socket>(handle).state(),
            tcp::State::Listen
        );
        device.available = true;
        assert!(iface.process_tcp_ingress(
            Instant::from_millis(124),
            &mut device,
            &mut sockets,
            PacketMeta::default(),
            ip.clone(),
            &bytes,
        ));
        assert_eq!(
            sockets.get::<tcp::Socket>(handle).state(),
            tcp::State::SynReceived
        );
        iface.poll_egress(Instant::from_millis(124), &mut device, &mut sockets);
        let response = device.inner.tx_queue.pop_front().unwrap();
        let tcp_packet = TcpPacket::new_checked(&response[ip.header_len()..]).unwrap();
        let repr = TcpRepr::parse(
            &tcp_packet,
            &ip.dst_addr(),
            &ip.src_addr(),
            &ChecksumCapabilities::default(),
        )
        .unwrap();
        assert_eq!(repr.control, TcpControl::Syn);
        assert_eq!(
            repr.max_seg_size,
            Some((1280 - ip.header_len() - 20) as u16)
        );
    }
}

#[test]
fn tcp_transport_rejects_inconsistent_ip_metadata() {
    for mut ip in addresses() {
        let (mut iface, mut sockets, mut device) = setup(Medium::Ip);
        let bytes = segment(&mut ip);
        ip.set_payload_len(bytes.len() + 1);
        assert!(iface.process_tcp_ingress(
            Instant::ZERO,
            &mut device,
            &mut sockets,
            PacketMeta::default(),
            ip.clone(),
            &bytes,
        ));
        assert!(device.tx_queue.is_empty());
        ip.set_payload_len(bytes.len());
        match &mut ip {
            #[cfg(feature = "proto-ipv4")]
            IpRepr::Ipv4(ip) => ip.next_header = IpProtocol::Udp,
            #[cfg(feature = "proto-ipv6")]
            IpRepr::Ipv6(ip) => ip.next_header = IpProtocol::Udp,
        }
        assert!(iface.process_tcp_ingress(
            Instant::ZERO,
            &mut device,
            &mut sockets,
            PacketMeta::default(),
            ip,
            &bytes,
        ));
        assert!(device.tx_queue.is_empty());
    }
}

#[test]
fn tcp_transport_mtu_retry_preserves_sequence_and_avoids_neighbor_wait() {
    struct RetryDevice {
        local: IpEndpoint,
        remote: IpEndpoint,
        attempts: Vec<Vec<u8>>,
    }
    struct RetryToken<'a>(&'a mut RetryDevice);
    impl TxToken for RetryToken<'_> {
        fn deferred_ip_output(&self, _: IpVersion) -> bool {
            true
        }
        fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, _: usize, _: F) -> R {
            panic!("complete IP admission required")
        }
        fn consume_full_ip<F>(
            self,
            len: usize,
            _: PacketMeta,
            _: crate::phy::IpOutputClass,
            _: Option<u16>,
            emit: F,
        ) -> Result<(), crate::phy::IpOutputError>
        where
            F: FnOnce(&mut [u8]),
        {
            let mut bytes = vec![0; len];
            emit(&mut bytes);
            self.0.attempts.push(bytes);
            if self.0.attempts.len() == 1 {
                Err(crate::phy::IpOutputError::MtuRetry(1280))
            } else {
                Ok(())
            }
        }
    }
    impl Device for RetryDevice {
        type RxToken<'a> = crate::tests::RxToken;
        type TxToken<'a> = RetryToken<'a>;
        fn receive(&mut self, _: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
            None
        }
        fn transmit(&mut self, _: Instant) -> Option<Self::TxToken<'_>> {
            Some(RetryToken(self))
        }
        fn capabilities(&self) -> crate::phy::DeviceCapabilities {
            crate::phy::DeviceCapabilities {
                medium: Medium::Ip,
                max_transmission_unit: 1500,
                ..Default::default()
            }
        }
        fn outbound_tcp_mtu(&self, local: IpEndpoint, remote: IpEndpoint, _: PacketMeta) -> usize {
            assert_eq!(local, self.local);
            assert_eq!(remote, self.remote);
            1500
        }
        fn outbound_tcp_mtu_generation(&self) -> u64 {
            7
        }
    }
    for ip in addresses() {
        let (mut iface, mut sockets, _) = setup(Medium::Ip);
        let local = IpEndpoint::new(ip.src_addr(), 40000);
        let remote = IpEndpoint::new(ip.dst_addr(), 80);
        let mut device = RetryDevice {
            local,
            remote,
            attempts: Vec::new(),
        };
        let mut socket = tcp::Socket::new(
            tcp::SocketBuffer::new(vec![0; 256]),
            tcp::SocketBuffer::new(vec![0; 256]),
        );
        socket.connect(iface.context(), remote, local).unwrap();
        let handle = sockets.add(socket);
        iface.poll_egress(Instant::ZERO, &mut device, &mut sockets);
        let first = TcpPacket::new_checked(&device.attempts[0][ip.header_len()..]).unwrap();
        let seq = first.seq_number();
        assert!(!sockets
            .get::<tcp::Socket>(handle)
            .accepts_pmtu_quote(local, remote, seq, 0));
        // Same-time poll must reach TCP again, without neighbor_missing delay.
        iface.poll_egress(Instant::ZERO, &mut device, &mut sockets);
        assert_eq!(device.attempts.len(), 2);
        let second = TcpPacket::new_checked(&device.attempts[1][ip.header_len()..]).unwrap();
        assert_eq!(second.seq_number(), seq);
        let repr = TcpRepr::parse(
            &second,
            &local.addr,
            &remote.addr,
            &ChecksumCapabilities::default(),
        )
        .unwrap();
        assert_eq!(
            repr.max_seg_size,
            Some((1280 - ip.header_len() - 20) as u16)
        );
        assert!(sockets
            .get::<tcp::Socket>(handle)
            .accepts_pmtu_quote(local, remote, seq + 1, 0));
    }
}
