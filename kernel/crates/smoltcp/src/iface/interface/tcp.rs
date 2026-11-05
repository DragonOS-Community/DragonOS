use super::*;

use crate::socket::tcp::Socket;

impl InterfaceInner {
    pub fn process_tcp<'frame>(
        &mut self,
        sockets: &mut SocketSet,
        meta: PacketMeta,
        ip_repr: IpRepr,
        ip_payload: &'frame [u8],
    ) -> Option<Packet<'frame>> {
        let (src_addr, dst_addr) = (ip_repr.src_addr(), ip_repr.dst_addr());
        let tcp_packet = check!(TcpPacket::new_checked(ip_payload));
        let tcp_repr = check!(TcpRepr::parse(
            &tcp_packet,
            &src_addr,
            &dst_addr,
            &self.caps.checksum
        ));

        #[cfg(feature = "alloc")]
        {
            // IPv6 extension headers and IP fragments have already been removed.
            // Describe precisely the validated transport segment handed off.
            let mut transport_ip = ip_repr.clone();
            match &mut transport_ip {
                #[cfg(feature = "proto-ipv4")]
                IpRepr::Ipv4(ip) => ip.next_header = IpProtocol::Tcp,
                #[cfg(feature = "proto-ipv6")]
                IpRepr::Ipv6(ip) => ip.next_header = IpProtocol::Tcp,
            }
            transport_ip.set_payload_len(ip_payload.len());
            if sockets.handle_tcp_ingress(meta, &transport_ip, ip_payload)
                == crate::iface::TcpIngressResult::Consumed
            {
                return None;
            }
        }

        #[cfg(feature = "alloc")]
        if let Some(result) = sockets.process_tcp_sockets(self, meta, &ip_repr, &tcp_repr) {
            return result
                .map(|(tx, ip, tcp)| Packet::new(ip, IpPayload::Tcp(tcp)).with_tx_meta(tx));
        }

        for listening in [false, true] {
            for tcp_socket in sockets
                .items_mut()
                .filter_map(|i| Socket::downcast_mut(&mut i.socket))
            {
                if tcp_socket.is_listening() != listening {
                    continue;
                }
                #[cfg(feature = "alloc")]
                if !tcp_socket.is_listening() {
                    continue;
                }
                if tcp_socket.accepts_ingress(meta) && tcp_socket.accepts(self, &ip_repr, &tcp_repr)
                {
                    let tx_meta = tcp_socket.egress_meta();
                    return tcp_socket
                        .process_with_meta(self, meta, &ip_repr, &tcp_repr)
                        .map(|(ip, tcp)| {
                            Packet::new(ip, IpPayload::Tcp(tcp)).with_tx_meta(tx_meta)
                        });
                }
            }
        }

        #[cfg(feature = "alloc")]
        if tcp_repr.control == TcpControl::Syn
            && tcp_repr.ack_number.is_none()
            && sockets.tcp_is_listening(IpEndpoint::new(dst_addr, tcp_repr.dst_port), meta)
        {
            // A logical listener still exists, but all of its socket slots are
            // occupied. Let the peer retransmit instead of reporting a closed port.
            return None;
        }

        if tcp_repr.control == TcpControl::Rst
            || ip_repr.dst_addr().is_unspecified()
            || ip_repr.src_addr().is_unspecified()
        {
            // Never reply to a TCP RST packet with another TCP RST packet. We also never want to
            // send a TCP RST packet with unspecified addresses.
            None
        } else {
            // The packet wasn't handled by a socket, send a TCP RST packet.
            let (ip, tcp) = tcp::Socket::rst_reply(&ip_repr, &tcp_repr);
            Some(Packet::new(ip, IpPayload::Tcp(tcp)))
        }
    }
}

impl Interface {
    /// Process a TCP segment already classified as local by an external IP layer.
    ///
    /// The caller must perform IP validation, reassembly and local destination
    /// checks. `ip_repr` must describe the TCP segment without extension headers.
    /// TCP length and checksum are checked here using this interface's checksum
    /// capabilities. No interface address lookup is performed.
    ///
    /// Returns false only when no transmit token is available; no protocol state
    /// is changed and the caller should retain the input for retry. True includes
    /// invalid packets deliberately dropped. Reserve sufficient token capacity
    /// before calling: processing may generate an immediate TCP response.
    /// The socket set normally has no external ingress handler installed.
    pub fn process_tcp_ingress(
        &mut self,
        timestamp: Instant,
        device: &mut (impl Device + ?Sized),
        sockets: &mut SocketSet<'_, '_>,
        meta: PacketMeta,
        ip_repr: IpRepr,
        segment: &[u8],
    ) -> bool {
        if ip_repr.next_header() != IpProtocol::Tcp || ip_repr.payload_len() != segment.len() {
            return true;
        }
        let Some(token) = device.transmit(timestamp) else {
            return false;
        };
        self.inner.now = timestamp;
        #[cfg(feature = "alloc")]
        sockets.expire_tcp_time_wait(timestamp);
        if let Some(response) = self.inner.process_tcp(sockets, meta, ip_repr, segment) {
            let tx_meta = response.tx_meta();
            // Dispatch failure is a network drop, just as in ordinary ingress.
            let _ = self
                .inner
                .dispatch_ip(token, tx_meta, response, &mut self.fragmenter);
        }
        true
    }
}
