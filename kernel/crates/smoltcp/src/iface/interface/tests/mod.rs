#[cfg(feature = "proto-ipv4")]
mod ipv4;
#[cfg(feature = "proto-ipv6")]
mod ipv6;
#[cfg(feature = "proto-sixlowpan")]
mod sixlowpan;
#[cfg(all(feature = "alloc", feature = "socket-tcp", feature = "medium-ip"))]
mod tcp_listener;
#[cfg(all(feature = "alloc", feature = "socket-tcp", feature = "medium-ip"))]
mod tcp_transport;

#[allow(unused)]
use std::vec::Vec;

use crate::tests::setup;

use rstest::*;

use super::*;

use crate::iface::Interface;
use crate::phy::ChecksumCapabilities;
#[cfg(feature = "alloc")]
use crate::phy::Loopback;
use crate::time::Instant;

#[allow(unused)]
fn fill_slice(s: &mut [u8], val: u8) {
    for x in s.iter_mut() {
        *x = val
    }
}

#[allow(unused)]
fn recv_all(device: &mut crate::tests::TestingDevice, timestamp: Instant) -> Vec<Vec<u8>> {
    let mut pkts = Vec::new();
    while let Some(pkt) = device.tx_queue.pop_front() {
        pkts.push(pkt)
    }
    pkts
}

#[derive(Debug, PartialEq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
struct MockTxToken;

impl TxToken for MockTxToken {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let mut junk = [0; 1536];
        f(&mut junk[..len])
    }
}

#[test]
#[should_panic(expected = "The hardware address does not match the medium of the interface.")]
#[cfg(all(feature = "medium-ip", feature = "medium-ethernet", feature = "alloc"))]
fn test_new_panic() {
    let mut device = Loopback::new(Medium::Ethernet);
    let config = Config::new(HardwareAddress::Ip);
    Interface::new(config, &mut device, Instant::ZERO);
}

#[rstest]
#[case::ip(Medium::Ip, 1200)]
#[cfg(feature = "medium-ip")]
#[case::ethernet(Medium::Ethernet, 1214)]
#[cfg(feature = "medium-ethernet")]
fn runtime_ip_mtu_updates_cached_capabilities(
    #[case] medium: Medium,
    #[case] expected_frame_mtu: usize,
) {
    let (mut iface, _, _) = setup(medium);

    iface.set_ip_mtu(1200).unwrap();

    assert_eq!(iface.inner.ip_mtu(), 1200);
    assert_eq!(iface.inner.caps.max_transmission_unit, expected_frame_mtu);
}

#[test]
#[cfg(feature = "medium-ethernet")]
fn runtime_ip_mtu_rejects_frame_size_overflow_without_mutation() {
    let (mut iface, _, _) = setup(Medium::Ethernet);
    let before = iface.inner.caps.max_transmission_unit;

    assert_eq!(
        iface.set_ip_mtu(usize::MAX),
        Err(IpMtuError::FrameSizeOverflow)
    );
    assert_eq!(iface.inner.caps.max_transmission_unit, before);
}

#[rstest]
#[case::ip(Medium::Ip)]
#[cfg(feature = "medium-ip")]
#[case::ethernet(Medium::Ethernet)]
#[cfg(feature = "medium-ethernet")]
#[case::ieee802154(Medium::Ieee802154)]
#[cfg(feature = "medium-ieee802154")]
fn runtime_ip_mtu_rejects_unsafe_small_values_without_mutation(#[case] medium: Medium) {
    let (mut iface, _, _) = setup(medium);
    let before = iface.inner.caps.max_transmission_unit;

    assert_eq!(iface.set_ip_mtu(67), Err(IpMtuError::TooSmall));
    assert_eq!(iface.inner.caps.max_transmission_unit, before);
    assert_eq!(iface.set_ip_mtu(68), Ok(()));
    assert_eq!(iface.inner.ip_mtu(), 68);
}

#[rstest]
#[case::ip(Medium::Ip)]
#[cfg(feature = "medium-ip")]
#[case::ethernet(Medium::Ethernet)]
#[cfg(feature = "medium-ethernet")]
#[case::ieee802154(Medium::Ieee802154)]
#[cfg(feature = "medium-ieee802154")]
fn runtime_ip_mtu_rejects_values_above_device_capability_without_mutation(#[case] medium: Medium) {
    let (mut iface, _, _) = setup(medium);
    let frame_mtu = iface.inner.caps.max_transmission_unit;
    let max_ip_mtu = iface.inner.ip_mtu();

    assert_eq!(iface.set_ip_mtu(max_ip_mtu + 1), Err(IpMtuError::TooLarge));
    assert_eq!(iface.inner.caps.max_transmission_unit, frame_mtu);
}

#[cfg(feature = "socket-udp")]
#[rstest]
#[case::ip(Medium::Ip)]
#[cfg(feature = "medium-ip")]
#[case::ethernet(Medium::Ethernet)]
#[cfg(feature = "medium-ethernet")]
#[case::ieee802154(Medium::Ieee802154)]
#[cfg(feature = "medium-ieee802154")]
fn test_handle_udp_broadcast(#[case] medium: Medium) {
    use crate::socket::udp;
    use crate::wire::IpEndpoint;

    static UDP_PAYLOAD: [u8; 5] = [0x48, 0x65, 0x6c, 0x6c, 0x6f];

    let (mut iface, mut sockets, _device) = setup(medium);

    let rx_buffer = udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY], vec![0; 15]);
    let tx_buffer = udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY], vec![0; 15]);

    let udp_socket = udp::Socket::new(rx_buffer, tx_buffer);

    let mut udp_bytes = vec![0u8; 13];
    let mut packet = UdpPacket::new_unchecked(&mut udp_bytes);

    let socket_handle = sockets.add(udp_socket);

    #[cfg(feature = "proto-ipv6")]
    let src_ip = Ipv6Address::new(0xfe80, 0, 0, 0, 0, 0, 0, 1);
    #[cfg(all(not(feature = "proto-ipv6"), feature = "proto-ipv4"))]
    let src_ip = Ipv4Address::new(0x7f, 0x00, 0x00, 0x02);

    let udp_repr = UdpRepr {
        src_port: 67,
        dst_port: 68,
    };

    #[cfg(feature = "proto-ipv6")]
    let ip_repr = IpRepr::Ipv6(Ipv6Repr {
        src_addr: src_ip,
        dst_addr: IPV6_LINK_LOCAL_ALL_NODES,
        next_header: IpProtocol::Udp,
        payload_len: udp_repr.header_len() + UDP_PAYLOAD.len(),
        hop_limit: 0x40,
    });
    #[cfg(all(not(feature = "proto-ipv6"), feature = "proto-ipv4"))]
    let ip_repr = IpRepr::Ipv4(Ipv4Repr {
        src_addr: src_ip,
        dst_addr: Ipv4Address::BROADCAST,
        next_header: IpProtocol::Udp,
        payload_len: udp_repr.header_len() + UDP_PAYLOAD.len(),
        hop_limit: 0x40,
    });
    let dst_addr = ip_repr.dst_addr();

    // Bind the socket to port 68
    let socket = sockets.get_mut::<udp::Socket>(socket_handle);
    assert_eq!(socket.bind(68), Ok(()));
    assert!(!socket.can_recv());
    assert!(socket.can_send());

    udp_repr.emit(
        &mut packet,
        &ip_repr.src_addr(),
        &ip_repr.dst_addr(),
        UDP_PAYLOAD.len(),
        |buf| buf.copy_from_slice(&UDP_PAYLOAD),
        &ChecksumCapabilities::default(),
    );

    // Packet should be handled by bound UDP socket
    assert_eq!(
        iface.inner.process_udp(
            &mut sockets,
            PacketMeta::default(),
            ip_repr,
            packet.into_inner(),
            false,
        ),
        None
    );

    // Make sure the payload to the UDP packet processed by process_udp is
    // appended to the bound sockets rx_buffer
    let socket = sockets.get_mut::<udp::Socket>(socket_handle);
    assert!(socket.can_recv());
    assert_eq!(
        socket.recv(),
        Ok((
            &UDP_PAYLOAD[..],
            udp::UdpMetadata {
                local_address: Some(dst_addr),
                local_port: Some(68),
                ..IpEndpoint::new(src_ip.into(), 67).into()
            }
        ))
    );
}

#[cfg(all(
    feature = "alloc",
    feature = "medium-ip",
    feature = "proto-ipv4",
    feature = "socket-udp"
))]
#[derive(Debug)]
struct TestUdpIngressHandler {
    result: crate::iface::UdpIngressResult,
    calls: core::sync::atomic::AtomicUsize,
    expected_broadcast: bool,
}

#[cfg(all(
    feature = "alloc",
    feature = "medium-ip",
    feature = "proto-ipv4",
    feature = "socket-udp"
))]
impl crate::iface::UdpIngressHandler for TestUdpIngressHandler {
    fn handle_udp_ingress(
        &self,
        _meta: PacketMeta,
        ip_repr: &IpRepr,
        udp_repr: &UdpRepr,
        is_broadcast: bool,
        payload: &[u8],
    ) -> crate::iface::UdpIngressResult {
        assert_eq!(ip_repr.src_addr(), Ipv4Address::new(127, 0, 0, 2).into());
        assert_eq!(ip_repr.dst_addr(), Ipv4Address::new(127, 0, 0, 1).into());
        assert_eq!(udp_repr.src_port, 67);
        assert_eq!(udp_repr.dst_port, 68);
        assert_eq!(is_broadcast, self.expected_broadcast);
        assert_eq!(payload, b"hello");
        self.calls
            .fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        self.result
    }
}

#[cfg(all(
    feature = "alloc",
    feature = "medium-ip",
    feature = "proto-ipv4",
    feature = "socket-udp"
))]
fn udp_ingress_test_packet() -> (IpRepr, Vec<u8>) {
    let udp_repr = UdpRepr {
        src_port: 67,
        dst_port: 68,
    };
    let ip_repr = IpRepr::Ipv4(Ipv4Repr {
        src_addr: Ipv4Address::new(127, 0, 0, 2),
        dst_addr: Ipv4Address::new(127, 0, 0, 1),
        next_header: IpProtocol::Udp,
        payload_len: udp_repr.header_len() + 5,
        hop_limit: 64,
    });
    let mut bytes = vec![0; udp_repr.header_len() + 5];
    udp_repr.emit(
        &mut UdpPacket::new_unchecked(&mut bytes),
        &ip_repr.src_addr(),
        &ip_repr.dst_addr(),
        5,
        |payload| payload.copy_from_slice(b"hello"),
        &ChecksumCapabilities::default(),
    );
    (ip_repr, bytes)
}

#[test]
#[cfg(all(
    feature = "alloc",
    feature = "medium-ip",
    feature = "proto-ipv4",
    feature = "socket-udp"
))]
fn udp_ingress_handler_consumed_bypasses_socket_and_icmp() {
    use crate::socket::udp;
    use alloc::sync::Arc;

    let (mut iface, mut sockets, _) = setup(Medium::Ip);
    let handler = Arc::new(TestUdpIngressHandler {
        result: crate::iface::UdpIngressResult::Consumed,
        calls: core::sync::atomic::AtomicUsize::new(0),
        expected_broadcast: false,
    });
    sockets.set_udp_ingress_handler(Some(handler.clone()));

    let rx_buffer = udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY], vec![0; 15]);
    let tx_buffer = udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY], vec![0; 15]);
    let handle = sockets.add(udp::Socket::new(rx_buffer, tx_buffer));
    sockets.get_mut::<udp::Socket>(handle).bind(68).unwrap();

    let (ip_repr, packet) = udp_ingress_test_packet();
    assert_eq!(
        iface
            .inner
            .process_udp(&mut sockets, PacketMeta::default(), ip_repr, &packet, false),
        None
    );
    assert!(!sockets.get::<udp::Socket>(handle).can_recv());

    sockets.remove(handle);
    let (ip_repr, packet) = udp_ingress_test_packet();
    assert_eq!(
        iface
            .inner
            .process_udp(&mut sockets, PacketMeta::default(), ip_repr, &packet, false),
        None
    );
    assert_eq!(handler.calls.load(core::sync::atomic::Ordering::Relaxed), 2);
}

#[test]
#[cfg(all(
    feature = "alloc",
    feature = "medium-ip",
    feature = "proto-ipv4",
    feature = "socket-udp"
))]
fn udp_ingress_handler_not_handled_falls_back_to_socket() {
    use crate::socket::udp;
    use alloc::sync::Arc;

    let (mut iface, mut sockets, _) = setup(Medium::Ip);
    let handler = Arc::new(TestUdpIngressHandler {
        result: crate::iface::UdpIngressResult::NotHandled,
        calls: core::sync::atomic::AtomicUsize::new(0),
        expected_broadcast: false,
    });
    sockets.set_udp_ingress_handler(Some(handler.clone()));

    let rx_buffer = udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY], vec![0; 15]);
    let tx_buffer = udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY], vec![0; 15]);
    let handle = sockets.add(udp::Socket::new(rx_buffer, tx_buffer));
    sockets.get_mut::<udp::Socket>(handle).bind(68).unwrap();

    let (ip_repr, packet) = udp_ingress_test_packet();
    assert_eq!(
        iface
            .inner
            .process_udp(&mut sockets, PacketMeta::default(), ip_repr, &packet, false),
        None
    );
    assert_eq!(handler.calls.load(core::sync::atomic::Ordering::Relaxed), 1);
    assert_eq!(
        sockets.get_mut::<udp::Socket>(handle).recv().unwrap().0,
        b"hello"
    );
}

#[test]
#[cfg(all(
    feature = "alloc",
    feature = "medium-ip",
    feature = "proto-ipv4",
    feature = "socket-udp"
))]
fn udp_ingress_handler_uses_explicit_broadcast_route_verdict() {
    use alloc::sync::Arc;

    let (mut iface, mut sockets, _) = setup(Medium::Ip);
    let handler = Arc::new(TestUdpIngressHandler {
        result: crate::iface::UdpIngressResult::NotHandled,
        calls: core::sync::atomic::AtomicUsize::new(0),
        expected_broadcast: true,
    });
    sockets.set_udp_ingress_handler(Some(handler.clone()));
    let (ip_repr, packet) = udp_ingress_test_packet();
    // The octets look unicast, but the ingress FIB selected RTN_BROADCAST.
    // With no listener, an ICMP Port Unreachable must not be generated.
    assert_eq!(
        iface
            .inner
            .process_udp(&mut sockets, PacketMeta::default(), ip_repr, &packet, true),
        None
    );
    assert_eq!(handler.calls.load(core::sync::atomic::Ordering::Relaxed), 1);
}

#[test]
#[cfg(all(feature = "medium-ip", feature = "socket-tcp", feature = "proto-ipv6"))]
pub fn tcp_not_accepted() {
    let (mut iface, mut sockets, _) = setup(Medium::Ip);
    let tcp = TcpRepr {
        src_port: 4242,
        dst_port: 4243,
        control: TcpControl::Syn,
        seq_number: TcpSeqNumber(-10001),
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

    let mut tcp_bytes = vec![0u8; tcp.buffer_len()];

    tcp.emit(
        &mut TcpPacket::new_unchecked(&mut tcp_bytes),
        &Ipv6Address::new(0xfe80, 0, 0, 0, 0, 0, 0, 2).into(),
        &Ipv6Address::new(0xfe80, 0, 0, 0, 0, 0, 0, 1).into(),
        &ChecksumCapabilities::default(),
    );

    assert_eq!(
        iface.inner.process_tcp(
            &mut sockets,
            PacketMeta::default(),
            IpRepr::Ipv6(Ipv6Repr {
                src_addr: Ipv6Address::new(0xfe80, 0, 0, 0, 0, 0, 0, 2),
                dst_addr: Ipv6Address::new(0xfe80, 0, 0, 0, 0, 0, 0, 1),
                next_header: IpProtocol::Tcp,
                payload_len: tcp.buffer_len(),
                hop_limit: 64,
            }),
            &tcp_bytes,
        ),
        Some(Packet::new_ipv6(
            Ipv6Repr {
                src_addr: Ipv6Address::new(0xfe80, 0, 0, 0, 0, 0, 0, 1),
                dst_addr: Ipv6Address::new(0xfe80, 0, 0, 0, 0, 0, 0, 2),
                next_header: IpProtocol::Tcp,
                payload_len: tcp.buffer_len(),
                hop_limit: 64,
            },
            IpPayload::Tcp(TcpRepr {
                src_port: 4243,
                dst_port: 4242,
                control: TcpControl::Rst,
                seq_number: TcpSeqNumber(0),
                ack_number: Some(TcpSeqNumber(-10000)),
                urgent_at: None,
                window_len: 0,
                window_scale: None,
                max_seg_size: None,
                sack_permitted: false,
                sack_ranges: [None, None, None],
                timestamp: None,
                payload: &[],
            })
        ))
    );
    // Unspecified destination address.
    tcp.emit(
        &mut TcpPacket::new_unchecked(&mut tcp_bytes),
        &Ipv6Address::new(0xfe80, 0, 0, 0, 0, 0, 0, 2).into(),
        &Ipv6Address::UNSPECIFIED.into(),
        &ChecksumCapabilities::default(),
    );

    assert_eq!(
        iface.inner.process_tcp(
            &mut sockets,
            PacketMeta::default(),
            IpRepr::Ipv6(Ipv6Repr {
                src_addr: Ipv6Address::new(0xfe80, 0, 0, 0, 0, 0, 0, 2),
                dst_addr: Ipv6Address::UNSPECIFIED,
                next_header: IpProtocol::Tcp,
                payload_len: tcp.buffer_len(),
                hop_limit: 64,
            }),
            &tcp_bytes,
        ),
        None,
    );
}
