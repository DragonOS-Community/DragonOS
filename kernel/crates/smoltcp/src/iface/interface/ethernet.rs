use super::*;

impl InterfaceInner {
    pub(super) fn process_ethernet<'frame>(
        &mut self,
        sockets: &mut SocketSet,
        meta: crate::phy::PacketMeta,
        frame: &'frame [u8],
        fragments: &'frame mut FragmentsBuffer,
    ) -> Option<EthernetPacket<'frame>> {
        self.process_ethernet_inner(
            sockets,
            meta,
            frame,
            fragments,
            #[cfg(feature = "alloc")]
            None,
        )
    }

    #[cfg(feature = "alloc")]
    pub(super) fn process_ethernet_filtered<'frame>(
        &mut self,
        sockets: &mut SocketSet,
        meta: crate::phy::PacketMeta,
        frame: &'frame [u8],
        fragments: &'frame mut FragmentsBuffer,
        scratch: &'frame mut crate::storage::PacketBytes,
        filter: &mut dyn IpIngressFilter,
    ) -> Option<EthernetPacket<'frame>> {
        self.process_ethernet_inner(sockets, meta, frame, fragments, Some((scratch, filter)))
    }

    fn process_ethernet_inner<'frame>(
        &mut self,
        sockets: &mut SocketSet,
        meta: crate::phy::PacketMeta,
        frame: &'frame [u8],
        fragments: &'frame mut FragmentsBuffer,
        #[cfg(feature = "alloc")] filter: Option<(
            &'frame mut crate::storage::PacketBytes,
            &mut dyn IpIngressFilter,
        )>,
    ) -> Option<EthernetPacket<'frame>> {
        let eth_frame = check!(EthernetFrame::new_checked(frame));

        // Ignore any packets not directed to our hardware address or any of the multicast groups.
        if !eth_frame.dst_addr().is_broadcast()
            && !eth_frame.dst_addr().is_multicast()
            && HardwareAddress::Ethernet(eth_frame.dst_addr()) != self.hardware_addr
        {
            return None;
        }

        match eth_frame.ethertype() {
            #[cfg(feature = "proto-ipv4")]
            EthernetProtocol::Arp => self.process_arp(
                self.now,
                &eth_frame,
                #[cfg(feature = "alloc")]
                filter.map(|(_, filter)| &*filter),
            ),
            #[cfg(feature = "proto-ipv4")]
            EthernetProtocol::Ipv4 => {
                let ipv4_packet = check!(Ipv4Packet::new_checked(eth_frame.payload()));
                #[cfg(feature = "alloc")]
                if let Some((scratch, filter)) = filter.filter(|(_, filter)| {
                    filter.applies_to_packet(IpVersion::Ipv4, ipv4_packet.as_ref())
                }) {
                    return self
                        .process_ipv4_filtered(
                            sockets,
                            meta,
                            eth_frame.src_addr().into(),
                            &ipv4_packet,
                            fragments,
                            scratch,
                            filter,
                        )
                        .map(EthernetPacket::Ip);
                }
                self.process_ipv4(
                    sockets,
                    meta,
                    eth_frame.src_addr().into(),
                    &ipv4_packet,
                    fragments,
                )
                .map(EthernetPacket::Ip)
            }
            #[cfg(feature = "proto-ipv6")]
            EthernetProtocol::Ipv6 => {
                let ipv6_packet = check!(Ipv6Packet::new_checked(eth_frame.payload()));
                #[cfg(feature = "alloc")]
                if let Some((scratch, filter)) = filter.filter(|(_, filter)| {
                    filter.applies_to_packet(IpVersion::Ipv6, ipv6_packet.as_ref())
                }) {
                    return self
                        .process_ipv6_filtered(
                            sockets,
                            meta,
                            eth_frame.src_addr().into(),
                            &ipv6_packet,
                            scratch,
                            filter,
                        )
                        .map(EthernetPacket::Ip);
                }
                self.process_ipv6(sockets, meta, eth_frame.src_addr().into(), &ipv6_packet)
                    .map(EthernetPacket::Ip)
            }
            // Drop all other traffic.
            _ => None,
        }
    }

    pub(super) fn dispatch_ethernet<Tx, F>(
        &mut self,
        mut tx_token: Tx,
        buffer_len: usize,
        f: F,
    ) -> Result<(), DispatchError>
    where
        Tx: TxToken,
        F: FnOnce(EthernetFrame<&mut [u8]>),
    {
        let tx_len = EthernetFrame::<&[u8]>::buffer_len(buffer_len);
        tx_token
            .prepare_buffer(tx_len)
            .map_err(|_| DispatchError::Exhausted)?;
        tx_token.consume(tx_len, |tx_buffer| {
            debug_assert!(tx_buffer.as_ref().len() == tx_len);
            let mut frame = EthernetFrame::new_unchecked(tx_buffer);

            let src_addr = self.hardware_addr.ethernet_or_panic();
            frame.set_src_addr(src_addr);

            f(frame);

            Ok(())
        })
    }
}
