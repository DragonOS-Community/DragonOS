use super::*;

#[cfg(all(feature = "alloc", feature = "socket-udp"))]
use crate::iface::UdpIngressResult;

#[cfg(feature = "socket-dns")]
use crate::socket::dns::Socket as DnsSocket;

#[cfg(feature = "socket-udp")]
use crate::socket::udp::Socket as UdpSocket;

impl InterfaceInner {
    pub(super) fn process_udp<'frame>(
        &mut self,
        sockets: &mut SocketSet,
        meta: PacketMeta,
        ip_repr: IpRepr,
        ip_payload: &'frame [u8],
        broadcast_route_selected: bool,
    ) -> Option<Packet<'frame>> {
        let (src_addr, dst_addr) = (ip_repr.src_addr(), ip_repr.dst_addr());
        let udp_packet = check!(UdpPacket::new_checked(ip_payload));
        let udp_repr = check!(UdpRepr::parse(
            &udp_packet,
            &src_addr,
            &dst_addr,
            &self.caps.checksum
        ));

        #[cfg(all(feature = "alloc", feature = "socket-udp"))]
        {
            let is_broadcast = match &ip_repr {
                #[cfg(feature = "proto-ipv4")]
                IpRepr::Ipv4(repr) => {
                    self.is_broadcast_v4(repr.dst_addr) || broadcast_route_selected
                }
                #[cfg(feature = "proto-ipv6")]
                IpRepr::Ipv6(_) => false,
            };
            if sockets.handle_udp_ingress(
                meta,
                &ip_repr,
                &udp_repr,
                is_broadcast,
                udp_packet.payload(),
            ) == UdpIngressResult::Consumed
            {
                return None;
            }
        }

        #[cfg(feature = "socket-udp")]
        for udp_socket in sockets
            .items_mut()
            .filter_map(|i| UdpSocket::downcast_mut(&mut i.socket))
        {
            if udp_socket.accepts(self, &ip_repr, &udp_repr) && udp_socket.accepts_ingress(meta) {
                udp_socket.process(self, meta, &ip_repr, &udp_repr, udp_packet.payload());
                return None;
            }
        }

        #[cfg(feature = "socket-dns")]
        for dns_socket in sockets
            .items_mut()
            .filter_map(|i| DnsSocket::downcast_mut(&mut i.socket))
        {
            if dns_socket.accepts(&ip_repr, &udp_repr) {
                dns_socket.process(self, &ip_repr, &udp_repr, udp_packet.payload());
                return None;
            }
        }

        // The packet wasn't handled by a socket. Broadcasts must not elicit
        // an ICMP error, whether classified by CIDR or by the ingress route.
        match ip_repr {
            #[cfg(feature = "proto-ipv4")]
            IpRepr::Ipv4(repr)
                if self.is_broadcast_v4(repr.dst_addr) || broadcast_route_selected =>
            {
                None
            }
            #[cfg(feature = "proto-ipv4")]
            IpRepr::Ipv4(ipv4_repr) => {
                let payload_len =
                    icmp_reply_payload_len(ip_payload.len(), IPV4_MIN_MTU, ipv4_repr.buffer_len());
                let icmpv4_reply_repr = Icmpv4Repr::DstUnreachable {
                    reason: Icmpv4DstUnreachable::PortUnreachable,
                    header: ipv4_repr,
                    data: &ip_payload[0..payload_len],
                };
                self.icmpv4_reply(ipv4_repr, icmpv4_reply_repr)
            }
            // RFC 4443 / Linux icmp6_send: an unbound UDP port must not
            // generate Port Unreachable for a multicast destination, even
            // when a RAW socket also received a copy of the datagram.
            #[cfg(feature = "proto-ipv6")]
            IpRepr::Ipv6(ipv6_repr) if ipv6_repr.dst_addr.is_multicast() => None,
            #[cfg(feature = "proto-ipv6")]
            IpRepr::Ipv6(ipv6_repr) => {
                let payload_len =
                    icmp_reply_payload_len(ip_payload.len(), IPV6_MIN_MTU, ipv6_repr.buffer_len());
                let icmpv6_reply_repr = Icmpv6Repr::DstUnreachable {
                    reason: Icmpv6DstUnreachable::PortUnreachable,
                    header: ipv6_repr,
                    data: &ip_payload[0..payload_len],
                };
                self.icmpv6_reply(ipv6_repr, icmpv6_reply_repr)
            }
        }
    }
}
