use super::*;

impl Interface {
    /// Process fragments that still need to be sent for IPv4 packets.
    ///
    /// This function returns a boolean value indicating whether any packets were
    /// processed or emitted, and thus, whether the readiness of any socket might
    /// have changed.
    #[cfg(feature = "proto-ipv4-fragmentation")]
    pub(super) fn ipv4_egress(&mut self, device: &mut (impl Device + ?Sized)) {
        // Reset the buffer when we transmitted everything.
        if self.fragmenter.finished() {
            self.fragmenter.reset();
        }

        if self.fragmenter.is_empty() {
            return;
        }

        let pkt = &self.fragmenter;
        if pkt.packet_len > pkt.sent_bytes {
            if let Some(tx_token) = device.transmit(self.inner.now) {
                self.inner
                    .dispatch_ipv4_frag(tx_token, &mut self.fragmenter);
            }
        }
    }
}

impl InterfaceInner {
    /// Get the next IPv4 fragment identifier.
    #[cfg(feature = "proto-ipv4-fragmentation")]
    pub(super) fn next_ipv4_frag_ident(&mut self) -> u16 {
        let ipv4_id = self.ipv4_id;
        self.ipv4_id = self.ipv4_id.wrapping_add(1);
        ipv4_id
    }

    /// Get an IPv4 source address based on a destination address.
    ///
    /// **NOTE**: unlike for IPv6, no specific selection algorithm is implemented. The first IPv4
    /// address from the interface is returned.
    #[allow(unused)]
    pub fn get_source_address_ipv4(&self, _dst_addr: &Ipv4Address) -> Option<Ipv4Address> {
        for cidr in self.ip_addrs.iter() {
            #[allow(irrefutable_let_patterns)] // if only ipv4 is enabled
            if let IpCidr::Ipv4(cidr) = cidr {
                return Some(cidr.address());
            }
        }
        None
    }

    /// ARP announces an authorized local packet source first. For forwarded
    /// traffic, choose an interface address in the next hop's prefix, then
    /// the first IPv4 address. AnyIP admission is not address ownership.
    #[cfg(feature = "medium-ethernet")]
    pub(super) fn get_arp_source_address(
        &self,
        next_hop: Ipv4Address,
        authorized_source: Option<IpAddress>,
    ) -> Option<Ipv4Address> {
        if let Some(IpAddress::Ipv4(source)) = authorized_source {
            if source.x_is_unicast() {
                return Some(source);
            }
        }
        let mut fallback = None;
        for cidr in self.ip_addrs.iter() {
            if let IpCidr::Ipv4(cidr) = cidr {
                fallback.get_or_insert(cidr.address());
                if cidr.contains_addr(&next_hop) {
                    return Some(cidr.address());
                }
            }
        }
        fallback
    }

    /// Checks if an address is broadcast, taking into account ipv4 subnet-local
    /// broadcast addresses.
    pub fn is_broadcast_v4(&self, address: Ipv4Address) -> bool {
        if address.is_broadcast() {
            return true;
        }

        self.ip_addrs
            .iter()
            .filter_map(|own_cidr| match own_cidr {
                IpCidr::Ipv4(own_ip) => Some(own_ip.broadcast()?),
                #[cfg(feature = "proto-ipv6")]
                IpCidr::Ipv6(_) => None,
            })
            .any(|broadcast_address| address == broadcast_address)
    }

    /// Checks if an ipv4 address is unicast, taking into account subnet broadcast addresses
    fn is_unicast_v4(&self, address: Ipv4Address) -> bool {
        address.x_is_unicast() && !self.is_broadcast_v4(address)
    }

    /// Whether the normal local protocol stack may receive this destination.
    /// DHCP is handled separately before this decision, since a client can
    /// receive its offer before the offered address belongs to the interface.
    fn accepts_local_ipv4_destination(&self, address: Ipv4Address) -> bool {
        if self.has_ip_addr(address)
            || self.has_multicast_group(address)
            || self.is_broadcast_v4(address)
        {
            return true;
        }

        self.any_ip
            && address.x_is_unicast()
            && self
                .routes
                .lookup(&IpAddress::Ipv4(address), self.now)
                .is_some_and(|router_addr| self.has_ip_addr(router_addr))
    }

    /// Get the first IPv4 address of the interface.
    pub fn ipv4_addr(&self) -> Option<Ipv4Address> {
        self.ip_addrs.iter().find_map(|addr| match *addr {
            IpCidr::Ipv4(cidr) => Some(cidr.address()),
            #[allow(unreachable_patterns)]
            _ => None,
        })
    }

    pub(super) fn process_ipv4<'a>(
        &mut self,
        sockets: &mut SocketSet,
        meta: PacketMeta,
        source_hardware_addr: HardwareAddress,
        ipv4_packet: &Ipv4Packet<&'a [u8]>,
        frag: &'a mut FragmentsBuffer,
    ) -> Option<Packet<'a>> {
        self.process_ipv4_inner(
            sockets,
            meta,
            source_hardware_addr,
            ipv4_packet,
            frag,
            #[cfg(feature = "alloc")]
            None,
            #[cfg(feature = "alloc")]
            None,
        )
    }

    #[cfg(feature = "alloc")]
    #[allow(clippy::too_many_arguments)] // Packet parsing inputs plus optional policy state.
    pub(super) fn process_ipv4_filtered<'a>(
        &mut self,
        sockets: &mut SocketSet,
        meta: PacketMeta,
        source_hardware_addr: HardwareAddress,
        ipv4_packet: &Ipv4Packet<&'a [u8]>,
        frag: &'a mut FragmentsBuffer,
        scratch: &'a mut crate::storage::PacketBytes,
        filter: &mut dyn IpIngressFilter,
    ) -> Option<Packet<'a>> {
        self.process_ipv4_inner(
            sockets,
            meta,
            source_hardware_addr,
            ipv4_packet,
            frag,
            Some(scratch),
            Some(filter),
        )
    }

    #[allow(clippy::too_many_arguments)] // Shared unfiltered and policy-aware packet path.
    fn process_ipv4_inner<'a>(
        &mut self,
        sockets: &mut SocketSet,
        meta: PacketMeta,
        source_hardware_addr: HardwareAddress,
        ipv4_packet: &Ipv4Packet<&'a [u8]>,
        frag: &'a mut FragmentsBuffer,
        #[cfg(feature = "alloc")] mut scratch: Option<&'a mut crate::storage::PacketBytes>,
        #[cfg(feature = "alloc")] mut filter: Option<&mut dyn IpIngressFilter>,
    ) -> Option<Packet<'a>> {
        let ipv4_repr = check!(Ipv4Repr::parse(ipv4_packet, &self.caps.checksum));
        #[cfg(feature = "proto-ipv4-fragmentation")]
        let mut source_hardware_addr = source_hardware_addr;
        #[cfg(feature = "proto-ipv4-fragmentation")]
        let mut ipv4_repr = ipv4_repr;
        #[cfg(not(feature = "proto-ipv4-fragmentation"))]
        let mut ipv4_repr = ipv4_repr;
        #[cfg(all(feature = "alloc", feature = "proto-ipv4-fragmentation"))]
        let mut first_fragment_header: Option<([u8; 60], usize)> = None;
        #[cfg(all(feature = "alloc", feature = "proto-ipv4-fragmentation"))]
        let mut first_fragment_mark: Option<u32> = None;
        #[cfg(all(feature = "alloc", feature = "proto-ipv4-fragmentation"))]
        let mut fragment_pre_routing_done = false;
        // Preserve the pre-existing early rejection on callers with no
        // pre-routing policy. A filtered caller must be able to observe the
        // packet at PRE_ROUTING, but an unfiltered caller must not let a
        // spoofed non-unicast source consume fragment-assembler slots.
        #[cfg(feature = "alloc")]
        let reject_before_reassembly = filter.is_none();
        #[cfg(not(feature = "alloc"))]
        let reject_before_reassembly = true;
        if reject_before_reassembly
            && !self.is_unicast_v4(ipv4_repr.src_addr)
            && !ipv4_repr.src_addr.is_unspecified()
        {
            net_debug!("non-unicast or unspecified source address");
            return None;
        }
        #[cfg(all(feature = "alloc", feature = "proto-ipv4-fragmentation"))]
        if (ipv4_packet.more_frags() || ipv4_packet.frag_offset() != 0)
            && filter
                .as_ref()
                .is_some_and(|filter| !filter.defragment_ipv4())
        {
            // Route fragments individually only when no pre-routing rule
            // requires a reassembled transport header. Preserve the normal
            // assembler for packets routed to this local stack.
            if !self.is_unicast_v4(ipv4_repr.src_addr) && !ipv4_repr.src_addr.is_unspecified() {
                return None;
            }
            let scratch = scratch.as_deref_mut()?;
            let filter = filter.as_deref_mut()?;
            let bytes = ipv4_packet.clone().into_inner();
            let mut packet =
                IngressPacket::borrowed(&bytes[..ipv4_packet.total_len() as usize], scratch);
            match filter.route_fragment(
                &mut RoutedIngressPacket::new(&mut packet),
                meta,
                source_hardware_addr,
            ) {
                RouteInputVerdict::Pass => fragment_pre_routing_done = true,
                RouteInputVerdict::Drop
                | RouteInputVerdict::Forward
                | RouteInputVerdict::NeighborDiscovery => return None,
            }
        }
        #[cfg(feature = "proto-ipv4-fragmentation")]
        let ip_payload = {
            if ipv4_packet.more_frags() || ipv4_packet.frag_offset() != 0 {
                let fragment_offset = ipv4_packet.frag_offset() as usize;
                let fragment_payload = ipv4_packet.payload();
                if ipv4_packet.more_frags() && fragment_payload.len() % 8 != 0 {
                    net_debug!("non-final IPv4 fragment payload is not eight-byte aligned");
                    return None;
                }
                let fragment_end = fragment_offset.checked_add(fragment_payload.len())?;
                let max_payload_len = u16::MAX as usize - crate::wire::ipv4::HEADER_LEN;
                if fragment_end > max_payload_len {
                    net_debug!("IPv4 fragment exceeds the maximum datagram length");
                    return None;
                }
                let key = FragKey::Ipv4(ipv4_packet.get_key());

                let f = match frag.assembler.get(&key, self.now + frag.reassembly_timeout) {
                    Ok(f) => f,
                    Err(_) => {
                        net_debug!("No available packet assembler for fragmented packet");
                        return None;
                    }
                };

                if fragment_payload.is_empty() {
                    f.reset();
                    return None;
                }
                if f.total_size().is_some_and(|total| {
                    fragment_end > total || (!ipv4_packet.more_frags() && fragment_end != total)
                }) || (!ipv4_packet.more_frags() && f.last_received_end() > fragment_end)
                {
                    f.reset();
                    return None;
                }

                if !ipv4_packet.more_frags() {
                    // A valid last length is recorded even if all of this
                    // fragment's bytes were previously received.
                    if f.set_total_size(fragment_end).is_err() {
                        f.reset();
                        return None;
                    }
                }

                match f.classify_received_range(fragment_offset, fragment_end) {
                    FragmentRange::New => {
                        if let Err(e) = f.add(fragment_payload, fragment_offset) {
                            net_debug!("fragmentation error: {:?}", e);
                            f.reset();
                            return None;
                        }

                        if fragment_offset == 0 {
                            f.set_first_ipv4_header(
                                &ipv4_packet.as_ref()[..ipv4_packet.header_len() as usize],
                                source_hardware_addr,
                                {
                                    #[cfg(feature = "alloc")]
                                    {
                                        filter.as_ref().map_or(0, |filter| filter.packet_mark())
                                    }
                                    #[cfg(not(feature = "alloc"))]
                                    {
                                        0
                                    }
                                },
                            );
                        }
                    }
                    // Linux records a final length but does not reassemble on a
                    // duplicate: no new bytes have entered the queue.
                    FragmentRange::Duplicate => return None,
                    FragmentRange::Overlap => {
                        f.reset();
                        net_debug!("overlapping IPv4 fragment discarded");
                        return None;
                    }
                }

                let first_header = f.first_ipv4_header()?;
                source_hardware_addr = f.first_ipv4_source_hardware_addr()?;
                #[cfg(feature = "alloc")]
                if filter.is_some() {
                    first_fragment_mark = f.first_ipv4_mark();
                }
                let first_hop_limit = first_header[8];
                let first_header_len = first_header.len();
                #[cfg(feature = "alloc")]
                if filter.is_some() {
                    let mut header = [0; 60];
                    header[..first_header_len].copy_from_slice(first_header);
                    first_fragment_header = Some((header, first_header_len));
                }
                match f.assemble() {
                    Some(payload) => {
                        // The parsed header may belong to the last fragment.
                        // Raw sockets and ICMP replies need the full length.
                        if payload.len() > u16::MAX as usize - first_header_len {
                            return None;
                        }
                        ipv4_repr.payload_len = payload.len();
                        ipv4_repr.hop_limit = first_hop_limit;
                        payload
                    }
                    None => return None,
                }
            } else {
                ipv4_packet.payload()
            }
        };

        #[cfg(not(feature = "proto-ipv4-fragmentation"))]
        let ip_payload = ipv4_packet.payload();

        #[cfg(feature = "alloc")]
        let source_checked_in_filter = filter.is_some();
        #[cfg(feature = "alloc")]
        let (ip_payload, local_destination, external_raw) = if let Some(scratch) = scratch {
            let filter = filter.as_deref_mut()?;
            #[cfg(feature = "proto-ipv4-fragmentation")]
            let reassembled = first_fragment_header.is_some();
            #[cfg(not(feature = "proto-ipv4-fragmentation"))]
            let reassembled = false;
            let mut packet = if reassembled {
                #[cfg(feature = "proto-ipv4-fragmentation")]
                {
                    let (header, header_len) = first_fragment_header?;
                    let total_len = header_len.checked_add(ip_payload.len())?;
                    if total_len > u16::MAX as usize {
                        return None;
                    }
                    scratch.clear();
                    scratch.try_reserve_exact(total_len).ok()?;
                    scratch.extend_from_slice(&header[..header_len]);
                    scratch.extend_from_slice(ip_payload);
                    let mut reassembled = Ipv4Packet::new_unchecked(scratch.as_mut_slice());
                    reassembled.set_total_len(total_len as u16);
                    reassembled.set_more_frags(false);
                    reassembled.set_frag_offset(0);
                    reassembled.fill_checksum();
                    filter.restore_packet_mark(first_fragment_mark?);
                    IngressPacket::assembled(scratch)
                }
                #[cfg(not(feature = "proto-ipv4-fragmentation"))]
                unreachable!()
            } else {
                // A receive buffer may include Ethernet padding after the IP
                // total length. PRE_ROUTING must only expose the IP datagram.
                let bytes = ipv4_packet.clone().into_inner();
                IngressPacket::borrowed(&bytes[..ipv4_packet.total_len() as usize], scratch)
            };
            let fragment_pre_routing_done = {
                #[cfg(feature = "proto-ipv4-fragmentation")]
                {
                    fragment_pre_routing_done
                }
                #[cfg(not(feature = "proto-ipv4-fragmentation"))]
                {
                    false
                }
            };
            if !fragment_pre_routing_done {
                match filter.pre_routing(&mut packet, meta, source_hardware_addr) {
                    PreRoutingVerdict::Pass => {}
                    PreRoutingVerdict::Drop => return None,
                }
            }
            if !packet.is_borrowed() {
                let rewritten = Ipv4Packet::new_checked(packet.bytes()).ok()?;
                ipv4_repr = Ipv4Repr::parse(&rewritten, &self.caps.checksum).ok()?;
            }

            // The route callback must not be able to rewrite the source after
            // this check. It can only inspect or retain the validated packet.
            if !self.is_unicast_v4(ipv4_repr.src_addr) && !ipv4_repr.src_addr.is_unspecified() {
                net_debug!("non-unicast or unspecified source address");
                return None;
            }
            match filter.route_input(
                &mut RoutedIngressPacket::new(&mut packet),
                meta,
                source_hardware_addr,
            ) {
                RouteInputVerdict::Pass => {}
                RouteInputVerdict::Drop
                | RouteInputVerdict::Forward
                | RouteInputVerdict::NeighborDiscovery => return None,
            }
            // Route selection precedes LOCAL_IN. The hook may rewrite only
            // after this packet has qualified for local delivery; its final
            // bytes must then be parsed again before raw/transport demux.
            let local_destination = self.accepts_local_ipv4_destination(ipv4_repr.dst_addr)
                || filter.local_route_selected();
            let external_raw = if local_destination {
                let transport_offset = (usize::from(packet.bytes()[0]) & 0x0f) * 4;
                match filter.local_input(
                    &mut packet,
                    meta,
                    source_hardware_addr,
                    ipv4_repr.next_header,
                    transport_offset,
                ) {
                    LocalInputVerdict::Pass => None,
                    LocalInputVerdict::Drop => return None,
                    LocalInputVerdict::ExternalRaw { matched } => Some(matched),
                }
            } else {
                None
            };
            if !packet.is_borrowed() {
                let rewritten = Ipv4Packet::new_checked(packet.bytes()).ok()?;
                ipv4_repr = Ipv4Repr::parse(&rewritten, &self.caps.checksum).ok()?;
                if !self.is_unicast_v4(ipv4_repr.src_addr) && !ipv4_repr.src_addr.is_unspecified() {
                    return None;
                }
            }
            if local_destination
                && !self.accepts_local_ipv4_destination(ipv4_repr.dst_addr)
                && !filter.local_route_selected()
            {
                return None;
            }
            let bytes = packet.into_bytes().ok()?;
            (
                Ipv4Packet::new_checked(bytes).ok()?.payload(),
                local_destination,
                external_raw,
            )
        } else {
            (
                ip_payload,
                self.accepts_local_ipv4_destination(ipv4_repr.dst_addr)
                    || filter
                        .as_ref()
                        .is_some_and(|filter| filter.local_route_selected()),
                None,
            )
        };

        // Linux PRE_ROUTING runs after basic IP validation but before
        // source-address policy and local/forward routing decisions.
        #[cfg(feature = "alloc")]
        if !source_checked_in_filter
            && !self.is_unicast_v4(ipv4_repr.src_addr)
            && !ipv4_repr.src_addr.is_unspecified()
        {
            net_debug!("non-unicast or unspecified source address");
            return None;
        }

        let ip_repr = IpRepr::Ipv4(ipv4_repr);

        // Raw sockets belong to local delivery, not the forwarding path.
        // Keep DHCP's pre-address handling below, but do not expose a transit
        // packet to raw sockets before the local-destination decision.
        #[cfg(not(feature = "alloc"))]
        let local_destination = self.accepts_local_ipv4_destination(ipv4_repr.dst_addr);

        #[cfg(feature = "socket-raw")]
        let handled_by_raw_socket = if local_destination {
            #[cfg(feature = "alloc")]
            if let Some(matched) = external_raw {
                matched
            } else {
                self.raw_socket_filter(sockets, &ip_repr, ip_payload)
            }
            #[cfg(not(feature = "alloc"))]
            self.raw_socket_filter(sockets, &ip_repr, ip_payload)
        } else {
            false
        };
        #[cfg(not(feature = "socket-raw"))]
        let handled_by_raw_socket = false;

        #[cfg(feature = "socket-dhcpv4")]
        {
            use crate::socket::dhcpv4::Socket as Dhcpv4Socket;

            if ipv4_repr.next_header == IpProtocol::Udp
                && matches!(self.caps.medium, Medium::Ethernet)
            {
                let udp_packet = check!(UdpPacket::new_checked(ip_payload));
                if let Some(dhcp_socket) = sockets
                    .items_mut()
                    .find_map(|i| Dhcpv4Socket::downcast_mut(&mut i.socket))
                {
                    // First check for source and dest ports, then do `UdpRepr::parse` if they match.
                    // This way we avoid validating the UDP checksum twice for all non-DHCP UDP packets (one here, one in `process_udp`)
                    if udp_packet.src_port() == dhcp_socket.server_port
                        && udp_packet.dst_port() == dhcp_socket.client_port
                    {
                        let udp_repr = check!(UdpRepr::parse(
                            &udp_packet,
                            &ipv4_repr.src_addr.into(),
                            &ipv4_repr.dst_addr.into(),
                            &self.caps.checksum
                        ));
                        dhcp_socket.process(self, &ipv4_repr, &udp_repr, udp_packet.payload());
                        return None;
                    }
                }
            }
        }

        if !local_destination {
            net_trace!(
                "Rejecting IPv4 packet; {} is not a local destination",
                ipv4_repr.dst_addr
            );
            return None;
        }

        #[cfg(feature = "alloc")]
        let broadcast_route_selected = filter
            .as_ref()
            .is_some_and(|filter| filter.broadcast_route_selected());
        #[cfg(not(feature = "alloc"))]
        let broadcast_route_selected = false;
        let broadcast_destination =
            self.is_broadcast_v4(ipv4_repr.dst_addr) || broadcast_route_selected;
        #[cfg(feature = "medium-ethernet")]
        if self.is_unicast_v4(ipv4_repr.dst_addr) && !broadcast_route_selected {
            self.neighbor_cache.reset_expiry_if_existing(
                IpAddress::Ipv4(ipv4_repr.src_addr),
                source_hardware_addr,
                self.now,
            );
        }

        match ipv4_repr.next_header {
            IpProtocol::Icmp => {
                self.process_icmpv4(sockets, ipv4_repr, ip_payload, broadcast_route_selected)
            }

            #[cfg(feature = "multicast")]
            IpProtocol::Igmp => self.process_igmp(ipv4_repr, ip_payload),

            #[cfg(any(feature = "socket-udp", feature = "socket-dns"))]
            IpProtocol::Udp => {
                self.process_udp(sockets, meta, ip_repr, ip_payload, broadcast_route_selected)
            }

            #[cfg(feature = "socket-tcp")]
            IpProtocol::Tcp if broadcast_destination => None,

            #[cfg(feature = "socket-tcp")]
            IpProtocol::Tcp => self.process_tcp(sockets, meta, ip_repr, ip_payload),

            _ if handled_by_raw_socket => None,

            _ if broadcast_destination => None,

            _ => {
                // Send back as much of the original payload as we can.
                let payload_len =
                    icmp_reply_payload_len(ip_payload.len(), IPV4_MIN_MTU, ipv4_repr.buffer_len());
                let icmp_reply_repr = Icmpv4Repr::DstUnreachable {
                    reason: Icmpv4DstUnreachable::ProtoUnreachable,
                    header: ipv4_repr,
                    data: &ip_payload[0..payload_len],
                };
                self.icmpv4_reply(ipv4_repr, icmp_reply_repr)
            }
        }
    }

    #[cfg(feature = "medium-ethernet")]
    pub(super) fn process_arp<'frame>(
        &mut self,
        timestamp: Instant,
        eth_frame: &EthernetFrame<&'frame [u8]>,
        #[cfg(feature = "alloc")] filter: Option<&dyn IpIngressFilter>,
    ) -> Option<EthernetPacket<'frame>> {
        if !self.neighbor_discovery_enabled {
            return None;
        }

        let arp_packet = check!(ArpPacket::new_checked(eth_frame.payload()));
        let arp_repr = check!(ArpRepr::parse(&arp_packet));

        match arp_repr {
            ArpRepr::EthernetIpv4 {
                operation,
                source_hardware_addr,
                source_protocol_addr,
                target_protocol_addr,
                ..
            } => {
                // Reply ownership and learning are separate: an integration
                // may authorize weak-host replies without broadening the
                // interface's existing neighbor-learning policy.
                let learning_target = self.has_ip_addr(target_protocol_addr) || self.any_ip;
                #[cfg(feature = "alloc")]
                let ownership = filter.and_then(|filter| {
                    filter.arp_reply_allowed(source_protocol_addr, target_protocol_addr)
                });
                #[cfg(not(feature = "alloc"))]
                let ownership: Option<bool> = None;

                // Only process REQUEST and RESPONSE.
                if let ArpOperation::Unknown(_) = operation {
                    net_debug!("arp: unknown operation code");
                    return None;
                }

                // Only an explicitly authorized local request may use the
                // zero source address for duplicate-address detection. Never
                // learn that address, and preserve legacy standalone behavior.
                let dad_request = source_protocol_addr.is_unspecified()
                    && operation == ArpOperation::Request
                    && ownership == Some(true);
                if (!source_protocol_addr.x_is_unicast() && !dad_request)
                    || !source_hardware_addr.is_unicast()
                {
                    net_debug!("arp: non-unicast source address");
                    return None;
                }

                let same_network = self.in_same_network(&IpAddress::Ipv4(source_protocol_addr));

                // Fill the ARP cache from any ARP packet aimed at us (both request or response).
                // We fill from requests too because if someone is requesting our address they
                // are probably going to talk to us, so we avoid having to request their address
                // when we later reply to them.
                if learning_target && same_network && !dad_request {
                    self.neighbor_cache.fill(
                        source_protocol_addr.into(),
                        source_hardware_addr.into(),
                        timestamp,
                    );
                }

                if operation == ArpOperation::Request
                    && ownership.unwrap_or(learning_target && same_network)
                {
                    let src_hardware_addr = self.hardware_addr.ethernet_or_panic();

                    Some(EthernetPacket::Arp(ArpRepr::EthernetIpv4 {
                        operation: ArpOperation::Reply,
                        source_hardware_addr: src_hardware_addr,
                        source_protocol_addr: target_protocol_addr,
                        target_hardware_addr: source_hardware_addr,
                        target_protocol_addr: source_protocol_addr,
                    }))
                } else {
                    None
                }
            }
        }
    }

    pub(super) fn process_icmpv4<'frame>(
        &mut self,
        _sockets: &mut SocketSet,
        ip_repr: Ipv4Repr,
        ip_payload: &'frame [u8],
        broadcast_route_selected: bool,
    ) -> Option<Packet<'frame>> {
        let icmp_packet = check!(Icmpv4Packet::new_checked(ip_payload));
        let icmp_repr = check!(Icmpv4Repr::parse(&icmp_packet, &self.caps.checksum));

        #[cfg(feature = "socket-icmp")]
        let mut handled_by_icmp_socket = false;

        #[cfg(all(feature = "socket-icmp", feature = "proto-ipv4"))]
        for icmp_socket in _sockets
            .items_mut()
            .filter_map(|i| icmp::Socket::downcast_mut(&mut i.socket))
        {
            if icmp_socket.accepts_v4(self, &ip_repr, &icmp_repr) {
                icmp_socket.process_v4(self, &ip_repr, &icmp_repr);
                handled_by_icmp_socket = true;
            }
        }

        match icmp_repr {
            // Respond to echo requests.
            #[cfg(feature = "proto-ipv4")]
            Icmpv4Repr::EchoRequest {
                ident,
                seq_no,
                data,
            } => {
                if broadcast_route_selected {
                    return None;
                }
                let icmp_reply_repr = Icmpv4Repr::EchoReply {
                    ident,
                    seq_no,
                    data,
                };
                self.icmpv4_reply(ip_repr, icmp_reply_repr)
            }

            // Ignore any echo replies.
            Icmpv4Repr::EchoReply { .. } => None,

            // Don't report an error if a packet with unknown type
            // has been handled by an ICMP socket
            #[cfg(feature = "socket-icmp")]
            _ if handled_by_icmp_socket => None,

            // FIXME: do something correct here?
            _ => None,
        }
    }

    pub(super) fn icmpv4_reply<'frame, 'icmp: 'frame>(
        &self,
        ipv4_repr: Ipv4Repr,
        icmp_repr: Icmpv4Repr<'icmp>,
    ) -> Option<Packet<'frame>> {
        if !self.is_unicast_v4(ipv4_repr.src_addr) {
            // Do not send ICMP replies to non-unicast sources
            None
        } else if self.is_unicast_v4(ipv4_repr.dst_addr) {
            // Reply as normal when src_addr and dst_addr are both unicast
            let ipv4_reply_repr = Ipv4Repr {
                src_addr: ipv4_repr.dst_addr,
                dst_addr: ipv4_repr.src_addr,
                next_header: IpProtocol::Icmp,
                payload_len: icmp_repr.buffer_len(),
                hop_limit: 64,
            };
            Some(Packet::new_ipv4(
                ipv4_reply_repr,
                IpPayload::Icmpv4(icmp_repr),
            ))
        } else if self.is_broadcast_v4(ipv4_repr.dst_addr) {
            // Only reply to broadcasts for echo replies and not other ICMP messages
            match icmp_repr {
                Icmpv4Repr::EchoReply { .. } => match self.ipv4_addr() {
                    Some(src_addr) => {
                        let ipv4_reply_repr = Ipv4Repr {
                            src_addr,
                            dst_addr: ipv4_repr.src_addr,
                            next_header: IpProtocol::Icmp,
                            payload_len: icmp_repr.buffer_len(),
                            hop_limit: 64,
                        };
                        Some(Packet::new_ipv4(
                            ipv4_reply_repr,
                            IpPayload::Icmpv4(icmp_repr),
                        ))
                    }
                    None => None,
                },
                _ => None,
            }
        } else {
            None
        }
    }

    #[cfg(feature = "proto-ipv4-fragmentation")]
    pub(super) fn dispatch_ipv4_frag<Tx: TxToken>(
        &mut self,
        mut tx_token: Tx,
        frag: &mut Fragmenter,
    ) {
        let caps = self.caps.clone();
        let tx_medium = frag.ipv4.egress.map_or(caps.medium, |egress| egress.medium);
        if tx_token.apply_egress_override(frag.ipv4.egress).is_err() {
            return;
        }
        tx_token.set_meta(frag.ipv4.meta);

        let mtu_max = frag
            .ipv4
            .egress
            .map_or_else(|| self.ip_mtu(), |egress| egress.ip_mtu);
        let header_len = frag.ipv4.repr.buffer_len();
        if mtu_max <= header_len {
            frag.sent_bytes = frag.packet_len;
            return;
        }
        let remaining_payload = frag.packet_len - frag.sent_bytes;
        let max_payload_len = mtu_max - header_len;
        let payload_len = if remaining_payload > max_payload_len {
            max_payload_len & !7
        } else {
            remaining_payload
        };
        if payload_len == 0 {
            frag.sent_bytes = frag.packet_len;
            return;
        }
        let ip_len = header_len + payload_len;

        let more_frags = remaining_payload != payload_len;

        let mut tx_len = ip_len;
        #[cfg(feature = "medium-ethernet")]
        if matches!(tx_medium, Medium::Ethernet) {
            tx_len += EthernetFrame::<&[u8]>::header_len();
        }

        if tx_token.prepare_buffer(tx_len).is_err() {
            return;
        }
        frag.ipv4.repr.payload_len = payload_len;
        frag.sent_bytes += payload_len;

        // Emit function for the Ethernet header.
        #[cfg(feature = "medium-ethernet")]
        let emit_ethernet = |repr: &IpRepr, tx_buffer: &mut [u8]| {
            let mut frame = EthernetFrame::new_unchecked(tx_buffer);

            let src_addr = self.hardware_addr.ethernet_or_panic();
            frame.set_src_addr(src_addr);
            frame.set_dst_addr(frag.ipv4.dst_hardware_addr);

            match repr.version() {
                #[cfg(feature = "proto-ipv4")]
                IpVersion::Ipv4 => frame.set_ethertype(EthernetProtocol::Ipv4),
                #[cfg(feature = "proto-ipv6")]
                IpVersion::Ipv6 => frame.set_ethertype(EthernetProtocol::Ipv6),
            }
        };

        tx_token.consume(tx_len, |mut tx_buffer| {
            #[cfg(feature = "medium-ethernet")]
            if matches!(tx_medium, Medium::Ethernet) {
                emit_ethernet(&IpRepr::Ipv4(frag.ipv4.repr), tx_buffer);
                tx_buffer = &mut tx_buffer[EthernetFrame::<&[u8]>::header_len()..];
            }

            let mut packet =
                Ipv4Packet::new_unchecked(&mut tx_buffer[..frag.ipv4.repr.buffer_len()]);
            frag.ipv4.repr.emit(&mut packet, &caps.checksum);
            packet.set_ident(frag.ipv4.ident);
            packet.set_more_frags(more_frags);
            packet.set_dont_frag(false);
            packet.set_frag_offset(frag.ipv4.frag_offset);

            if caps.checksum.ipv4.tx() {
                packet.fill_checksum();
            }

            tx_buffer[frag.ipv4.repr.buffer_len()..][..payload_len].copy_from_slice(
                &frag.buffer[frag.ipv4.frag_offset as usize + frag.ipv4.repr.buffer_len()..]
                    [..payload_len],
            );

            // Update the frag offset for the next fragment.
            frag.ipv4.frag_offset += payload_len as u16;
        })
    }
}
