//! A single received IP packet that is copied only if a hook needs to rewrite it.

use crate::phy::PacketMeta;
use crate::storage::PacketBytes;
use crate::wire::{HardwareAddress, IpProtocol, IpVersion};

/// The packet could not be made writable. Callers must drop it, never pass
/// the original bytes on as if the rewrite had succeeded.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IngressPacketAllocError;

/// PRE_ROUTING cannot forward: source validation and routing have not run yet.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PreRoutingVerdict {
    Pass,
    Drop,
}

/// This verdict applies after the packet has passed source validation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RouteInputVerdict {
    Pass,
    Drop,
    /// The integration has retained an owned packet for deferred forwarding.
    Forward,
    /// IPv6 neighbor discovery for a weak-host local destination stays on
    /// the physical ingress interface. The receive stack must not dispatch
    /// transport sockets for an arbitrary nonlocal destination; it only
    /// bypasses the address-owner check for a candidate NDISC message. The
    /// ordinary ICMPv6 parser still validates the complete packet afterward.
    NeighborDiscovery,
}

/// The local-delivery hook runs after routing and before raw and transport
/// sockets. An external raw receiver may take over raw fanout while the
/// built-in transport stack still processes the packet normally.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LocalInputVerdict {
    Pass,
    Drop,
    ExternalRaw { matched: bool },
}

pub trait IpIngressFilter {
    /// Override ARP reply authorization with the integration's address policy.
    /// `None` preserves the interface's standalone `any_ip` behavior. This
    /// decision controls replies only, not neighbor-cache learning. A local
    /// address may belong to another interface in the same network namespace.
    /// Integrations must validate both the sender and target according to
    /// their routing policy, including any martian-source restrictions.
    #[cfg(feature = "proto-ipv4")]
    fn arp_reply_allowed(
        &self,
        _source: crate::wire::Ipv4Address,
        _target: crate::wire::Ipv4Address,
    ) -> Option<bool> {
        None
    }

    /// Stop before acquiring another RX token if the integration's prepared
    /// policy view has been replaced. The caller can then release its locks
    /// and rebuild the policy and routing views without consuming the packet.
    fn continue_ingress_poll(&self) -> bool {
        true
    }

    /// Metadata of the current receive token, retained with offset zero when
    /// local IPv4 reassembly creates a new packet object.
    fn packet_mark(&self) -> u32 {
        0
    }

    fn restore_packet_mark(&self, _mark: u32) {}

    /// Start one receive-token packet, including malformed or non-IP frames.
    /// A poll may consume multiple packets through the same filter. The
    /// integration must discard any per-packet state left by the prior token
    /// before this packet's PRE_ROUTING or fragment callback can run.
    fn begin_packet(&mut self, _meta: PacketMeta) {}

    /// Called for every received IP datagram, including when no rules are
    /// installed. A false result preserves the ordinary unfiltered receive
    /// path for that datagram. Integrations must not cache this decision for
    /// an entire poll loop because their ruleset may change between packets.
    fn applies_to(&self, _version: IpVersion) -> bool {
        true
    }

    /// Packet-aware fast-path selection. This lets integrations inspect an
    /// exceptional packet (for example an IPv6 fragment) without sending all
    /// packets of that family through the filtered receive path.
    fn applies_to_packet(&self, version: IpVersion, _packet: &[u8]) -> bool {
        self.applies_to(version)
    }

    /// Forwarding can inspect an individual IPv4 fragment without consuming
    /// a local reassembly slot when no installed policy needs the complete
    /// datagram. A locally delivered fragment still follows normal assembly.
    fn defragment_ipv4(&self) -> bool {
        true
    }

    /// Optional IPv6 Netfilter defragmentation stage. This runs after the
    /// receive-core IPv6/Hop-by-Hop checks but before ordinary PRE_ROUTING.
    /// A fragment consumer may retain an owned copy with `take_owned` and
    /// return Drop; the completed datagram must later re-enter at this same
    /// stage so its ordinary PRE_ROUTING hook still runs exactly once.
    /// Non-fragmented packets and callers without defragmentation pass through.
    fn pre_routing_ipv6_defrag(
        &mut self,
        _packet: &mut IngressPacket<'_>,
        _meta: PacketMeta,
        _source_hardware_addr: HardwareAddress,
    ) -> PreRoutingVerdict {
        PreRoutingVerdict::Pass
    }

    /// A trusted loopback receive token may carry a packet whose sendto route
    /// was local even though IP_HDRINCL supplied a different header daddr.
    /// Ordinary link ingress must never override the destination test.
    fn local_route_selected(&self) -> bool {
        false
    }

    /// The packet's ingress route classified its destination as broadcast.
    /// Integrations may know about explicitly configured broadcast addresses
    /// that cannot be inferred from this interface's CIDR list alone.
    fn broadcast_route_selected(&self) -> bool {
        false
    }

    /// `meta` comes from the trusted receive token. Integrations may use it
    /// to recover the ingress interface, but must not infer it from IP bytes.
    /// For fragmented IPv4 datagrams, `source_hardware_addr` belongs to the
    /// offset-zero fragment, not whichever fragment completed reassembly.
    fn pre_routing(
        &mut self,
        packet: &mut IngressPacket<'_>,
        meta: PacketMeta,
        source_hardware_addr: HardwareAddress,
    ) -> PreRoutingVerdict;

    /// Called for IPv4 and IPv6 after PRE_ROUTING rewrites have been parsed and the
    /// receive stack's source-address policy has run, but before raw sockets
    /// or local-delivery classification. Forward only after retaining the
    /// packet with `take_owned` for processing outside the interface lock.
    fn route_input(
        &mut self,
        _packet: &mut RoutedIngressPacket<'_, '_>,
        _meta: PacketMeta,
        _source_hardware_addr: HardwareAddress,
    ) -> RouteInputVerdict {
        RouteInputVerdict::Pass
    }

    /// Decide whether an IPv4 fragment can be forwarded without local
    /// reassembly. Implementations must apply any stateless PRE_ROUTING
    /// verdict before forwarding it, including when the fragment will be
    /// reassembled for local delivery. Returning Pass lets a local fragment
    /// enter the assembler without re-running PRE_ROUTING on completion. The default is
    /// fail-closed when an integration disables defragmentation without
    /// implementing fragment policy.
    fn route_fragment(
        &mut self,
        _packet: &mut RoutedIngressPacket<'_, '_>,
        _meta: PacketMeta,
        _source_hardware_addr: HardwareAddress,
    ) -> RouteInputVerdict {
        RouteInputVerdict::Drop
    }

    /// `packet` contains the validated, post-PRE_ROUTING IP datagram, without
    /// link-layer padding. It is borrowed only for this call. A hook that
    /// changes it must request `writable()` and repair all affected checksums.
    /// The receive stack revalidates the result before raw or transport
    /// delivery, including its final local-destination eligibility.
    /// `transport_offset` identifies the payload of `protocol` after any IPv6
    /// extension headers. A raw receiver must retain an owned copy before
    /// this call returns.
    fn local_input(
        &mut self,
        _packet: &mut IngressPacket<'_>,
        _meta: PacketMeta,
        _source_hardware_addr: HardwareAddress,
        _protocol: IpProtocol,
        _transport_offset: usize,
    ) -> LocalInputVerdict {
        LocalInputVerdict::Pass
    }
}

/// The routing decision sees the already-validated datagram. It can inspect
/// or retain it, but cannot rewrite addresses after source validation.
pub struct RoutedIngressPacket<'p, 'a> {
    packet: &'p mut IngressPacket<'a>,
}

impl<'p, 'a> RoutedIngressPacket<'p, 'a> {
    pub(crate) fn new(packet: &'p mut IngressPacket<'a>) -> Self {
        Self { packet }
    }

    pub fn bytes(&self) -> &[u8] {
        self.packet.bytes()
    }

    pub fn take_owned(&mut self) -> Result<PacketBytes, IngressPacketAllocError> {
        self.packet.take_owned()
    }
}

pub struct IngressPacket<'a> {
    original: &'a [u8],
    scratch: &'a mut PacketBytes,
    copied: bool,
    failed: bool,
    transferred: bool,
}

impl<'a> IngressPacket<'a> {
    pub(crate) fn borrowed(original: &'a [u8], scratch: &'a mut PacketBytes) -> Self {
        Self {
            original,
            scratch,
            copied: false,
            failed: false,
            transferred: false,
        }
    }

    /// Use bytes already assembled into scratch without an unnecessary second copy.
    pub(crate) fn assembled(scratch: &'a mut PacketBytes) -> Self {
        Self {
            original: &[],
            scratch,
            copied: true,
            failed: false,
            transferred: false,
        }
    }

    pub fn bytes(&self) -> &[u8] {
        if self.copied {
            self.scratch
        } else {
            self.original
        }
    }

    /// A read-only decision needs no copy or second IP-header parse.
    pub(crate) fn is_borrowed(&self) -> bool {
        !self.copied && !self.failed && !self.transferred
    }

    pub fn writable(&mut self) -> Result<&mut [u8], IngressPacketAllocError> {
        if self.failed || self.transferred {
            return Err(IngressPacketAllocError);
        }
        if !self.copied {
            self.scratch.clear();
            if self.scratch.try_reserve_exact(self.original.len()).is_err() {
                self.failed = true;
                return Err(IngressPacketAllocError);
            }
            self.scratch.extend_from_slice(self.original);
            self.copied = true;
        }
        Ok(self.scratch)
    }

    /// Move a rewritten or reassembled packet to an integration-owned work
    /// queue. Borrowed packets are copied once because the receive token may
    /// cease to exist when this callback returns. After transfer, the packet
    /// cannot be passed to the local socket demultiplexer.
    pub fn take_owned(&mut self) -> Result<PacketBytes, IngressPacketAllocError> {
        if self.failed || self.transferred {
            return Err(IngressPacketAllocError);
        }
        if !self.copied {
            self.writable()?;
        }
        self.transferred = true;
        Ok(self.scratch.take())
    }

    pub(crate) fn into_bytes(self) -> Result<&'a [u8], IngressPacketAllocError> {
        if self.failed || self.transferred {
            Err(IngressPacketAllocError)
        } else if self.copied {
            Ok(self.scratch)
        } else {
            Ok(self.original)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pass_borrows_original_and_rewrite_copies_once() {
        let original = [1, 2, 3, 4];
        let mut scratch = crate::storage::PacketBytes::new();
        let pass = IngressPacket::borrowed(&original, &mut scratch);
        assert!(core::ptr::eq(
            pass.into_bytes().unwrap().as_ptr(),
            original.as_ptr()
        ));
        assert!(scratch.is_empty());

        let mut rewrite = IngressPacket::borrowed(&original, &mut scratch);
        rewrite.writable().unwrap()[0] = 9;
        assert_eq!(rewrite.bytes(), &[9, 2, 3, 4]);
        assert_eq!(rewrite.into_bytes().unwrap(), &[9, 2, 3, 4]);
        assert_eq!(original, [1, 2, 3, 4]);
    }

    #[test]
    fn assembled_packet_is_mutated_in_place() {
        let mut scratch = crate::storage::PacketBytes::new();
        scratch.try_reserve_exact(3).unwrap();
        scratch.extend_from_slice(&[1, 2, 3]);
        let original = scratch.as_ptr();
        let mut packet = IngressPacket::assembled(&mut scratch);
        packet.writable().unwrap()[2] = 4;
        let bytes = packet.into_bytes().unwrap();
        assert_eq!(bytes, &[1, 2, 4]);
        assert!(core::ptr::eq(bytes.as_ptr(), original));
    }

    #[test]
    fn forwarding_moves_rewritten_buffer_and_invalidates_local_delivery() {
        let original = [1, 2, 3];
        let mut scratch = crate::storage::PacketBytes::new();
        let mut packet = IngressPacket::borrowed(&original, &mut scratch);
        packet.writable().unwrap()[0] = 9;
        let rewritten = packet.bytes().as_ptr();
        let forwarded = packet.take_owned().unwrap();
        assert!(core::ptr::eq(forwarded.as_ptr(), rewritten));
        assert_eq!(&forwarded[..], &[9, 2, 3]);
        assert_eq!(packet.writable(), Err(IngressPacketAllocError));
        assert!(matches!(packet.take_owned(), Err(IngressPacketAllocError)));
        assert_eq!(packet.into_bytes(), Err(IngressPacketAllocError));
        assert_eq!(original, [1, 2, 3]);
    }

    #[test]
    fn forwarding_borrowed_packet_copies_before_receive_token_expires() {
        let original = [1, 2, 3];
        let mut scratch = crate::storage::PacketBytes::new();
        let mut packet = IngressPacket::borrowed(&original, &mut scratch);
        assert_eq!(&packet.take_owned().unwrap()[..], &original);
        assert_eq!(packet.into_bytes(), Err(IngressPacketAllocError));
    }
}
