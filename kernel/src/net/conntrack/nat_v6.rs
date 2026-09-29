//! Stateless IPv6 NAT packet rewriting.
//!
//! Conntrack owns the mapping and hook order. This module only rewrites a
//! complete datagram matching a known tuple, or a RELATED ICMPv6 error that
//! quotes one. Structural and tuple checks precede every write.

use super::{
    nat::{NatManipSide, NatRewriteError},
    packet::{checksum, CtChecksumMode, ParsedCtPacket},
    packet_v6::{ipv6_header, parse_ipv6_conntrack_with_mode},
    CtAddress, CtL4, CtTuple,
};

enum L4Rewrite {
    Generic,
    Tcp {
        old_ports: [u16; 2],
        new_ports: [u16; 2],
        checksum_present: bool,
    },
    Udp {
        old_ports: [u16; 2],
        new_ports: [u16; 2],
        checksum_present: bool,
    },
    Icmpv6 {
        old_id: u16,
        new_id: u16,
    },
}

struct Ipv6AddressRewrite {
    old_src: [u8; 16],
    old_dst: [u8; 16],
    new_src: [u8; 16],
    new_dst: [u8; 16],
}

fn copy_available_prefix(destination: &mut [u8], replacement: &[u8]) {
    let len = destination.len().min(replacement.len());
    destination[..len].copy_from_slice(&replacement[..len]);
}

fn addresses(from: CtTuple, to: CtTuple) -> Result<Ipv6AddressRewrite, NatRewriteError> {
    match (from.src, from.dst, to.src, to.dst) {
        (
            CtAddress::V6(old_src),
            CtAddress::V6(old_dst),
            CtAddress::V6(new_src),
            CtAddress::V6(new_dst),
        ) => Ok(Ipv6AddressRewrite {
            old_src,
            old_dst,
            new_src,
            new_dst,
        }),
        _ => Err(NatRewriteError::Unsupported),
    }
}

/// Produce a complete, non-failing write plan before touching packet bytes.
/// Linux 6.6 `tcp_manip_pkt()` changes an eight-byte error quote's port but
/// cannot update the TCP checksum unless the quoted checksum field exists.
fn l4_plan(
    from: CtL4,
    to: CtL4,
    bytes: &[u8],
    quoted: bool,
    generated: bool,
) -> Result<L4Rewrite, NatRewriteError> {
    match (from, to) {
        (
            CtL4::Tcp {
                src_port: old_src,
                dst_port: old_dst,
            },
            CtL4::Tcp {
                src_port: new_src,
                dst_port: new_dst,
            },
        ) if generated || bytes.len() >= if quoted { 8 } else { 20 } => Ok(L4Rewrite::Tcp {
            old_ports: [old_src, old_dst],
            new_ports: [new_src, new_dst],
            checksum_present: bytes.len() >= 20,
        }),
        (
            CtL4::Udp {
                src_port: old_src,
                dst_port: old_dst,
            },
            CtL4::Udp {
                src_port: new_src,
                dst_port: new_dst,
            },
        ) if generated || bytes.len() >= 8 => Ok(L4Rewrite::Udp {
            old_ports: [old_src, old_dst],
            new_ports: [new_src, new_dst],
            checksum_present: bytes.len() >= 8 && (bytes[6] != 0 || bytes[7] != 0),
        }),
        (
            CtL4::Icmpv6 {
                identifier: old_id,
                kind: old_kind,
                code: old_code,
            },
            CtL4::Icmpv6 {
                identifier: new_id,
                kind: new_kind,
                code: new_code,
            },
        ) if (generated || bytes.len() >= 8)
            && old_kind == new_kind
            && old_code == new_code
            && (matches!(old_kind, 128 | 129) || old_id == new_id) =>
        {
            // Linux 6.6 changes the identifier only for echo request/reply.
            // Node Information uses the tuple's identifier slot for Qtype,
            // which is not a NAT-mutable echo identifier.
            Ok(L4Rewrite::Icmpv6 { old_id, new_id })
        }
        (CtL4::Generic { protocol: old }, CtL4::Generic { protocol: new }) if old == new => {
            Ok(L4Rewrite::Generic)
        }
        _ => Err(NatRewriteError::Unsupported),
    }
}

fn replace_word(check: u16, old: u16, new: u16) -> u16 {
    let mut sum = u32::from(!check) + u32::from(!old) + u32::from(new);
    sum = (sum & 0xffff) + (sum >> 16);
    sum = (sum & 0xffff) + (sum >> 16);
    !(sum as u16)
}

fn adjust_checksum(
    mut check: u16,
    old_src: [u8; 16],
    old_dst: [u8; 16],
    new_src: [u8; 16],
    new_dst: [u8; 16],
    old_fields: &[u16],
    new_fields: &[u16],
) -> u16 {
    for (old, new) in old_src
        .chunks_exact(2)
        .chain(old_dst.chunks_exact(2))
        .zip(new_src.chunks_exact(2).chain(new_dst.chunks_exact(2)))
    {
        check = replace_word(
            check,
            u16::from_be_bytes([old[0], old[1]]),
            u16::from_be_bytes([new[0], new[1]]),
        );
    }
    for (&old, &new) in old_fields.iter().zip(new_fields) {
        check = replace_word(check, old, new);
    }
    check
}

fn apply_l4(
    bytes: &mut [u8],
    plan: L4Rewrite,
    old_src: [u8; 16],
    old_dst: [u8; 16],
    new_src: [u8; 16],
    new_dst: [u8; 16],
) {
    match plan {
        L4Rewrite::Generic => {}
        L4Rewrite::Tcp {
            old_ports,
            new_ports,
            checksum_present,
        } => {
            if checksum_present {
                let prior = u16::from_be_bytes([bytes[16], bytes[17]]);
                let next = adjust_checksum(
                    prior, old_src, old_dst, new_src, new_dst, &old_ports, &new_ports,
                );
                bytes[16..18].copy_from_slice(&next.to_be_bytes());
            }
            let src = new_ports[0].to_be_bytes();
            let dst = new_ports[1].to_be_bytes();
            copy_available_prefix(bytes, &[src[0], src[1], dst[0], dst[1]]);
        }
        L4Rewrite::Udp {
            old_ports,
            new_ports,
            checksum_present,
        } => {
            if checksum_present {
                let prior = u16::from_be_bytes([bytes[6], bytes[7]]);
                let next = adjust_checksum(
                    prior, old_src, old_dst, new_src, new_dst, &old_ports, &new_ports,
                );
                bytes[6..8].copy_from_slice(&if next == 0 { u16::MAX } else { next }.to_be_bytes());
            }
            // Linux 6.6 preserves an absent UDP checksum, even though normal
            // IPv6 UDP socket delivery subsequently rejects it.
            let src = new_ports[0].to_be_bytes();
            let dst = new_ports[1].to_be_bytes();
            copy_available_prefix(bytes, &[src[0], src[1], dst[0], dst[1]]);
        }
        L4Rewrite::Icmpv6 { old_id, new_id } => {
            if bytes.len() < 6 {
                return;
            }
            let prior = u16::from_be_bytes([bytes[2], bytes[3]]);
            let next = adjust_checksum(
                prior,
                old_src,
                old_dst,
                new_src,
                new_dst,
                &[old_id],
                &[new_id],
            );
            bytes[2..4].copy_from_slice(&next.to_be_bytes());
            if old_id != new_id {
                bytes[4..6].copy_from_slice(&new_id.to_be_bytes());
            }
        }
    }
}

fn checksum_ipv6(payload: &[u8], src: [u8; 16], dst: [u8; 16], protocol: u8) -> u16 {
    let mut initial = 0u32;
    for pair in src.chunks_exact(2).chain(dst.chunks_exact(2)) {
        initial += u32::from(u16::from_be_bytes([pair[0], pair[1]]));
    }
    let length = payload.len() as u32;
    initial += (length >> 16) + (length & 0xffff) + u32::from(protocol);
    !checksum(payload, initial)
}

pub(crate) fn is_ipv6_related_redirect(packet: &[u8], mode: CtChecksumMode) -> bool {
    if !matches!(
        parse_ipv6_conntrack_with_mode(packet, mode),
        ParsedCtPacket::Related { .. }
    ) {
        return false;
    }
    ipv6_header(packet, false).is_some_and(|header| {
        header.protocol == 58 && packet.get(header.transport_offset) == Some(&137)
    })
}

/// Rewrite a complete IPv6 TCP, UDP or tracked ICMPv6 query packet.
/// `Skip` skips input checksum verification only; it still checks lengths,
/// extension headers and the exact incoming tuple. It does not repair a bad
/// incoming checksum or emulate skb `CHECKSUM_PARTIAL` offload metadata.
pub(crate) fn rewrite_ipv6_tuple(
    packet: &mut [u8],
    from: CtTuple,
    to: CtTuple,
    mode: CtChecksumMode,
) -> Result<(), NatRewriteError> {
    let Ipv6AddressRewrite {
        old_src,
        old_dst,
        new_src,
        new_dst,
    } = addresses(from, to)?;
    let ParsedCtPacket::Flow { tuple, .. } = parse_ipv6_conntrack_with_mode(packet, mode) else {
        return Err(NatRewriteError::InvalidPacket);
    };
    if tuple != from {
        return Err(NatRewriteError::TupleMismatch);
    }
    let header = ipv6_header(packet, false).ok_or(NatRewriteError::InvalidPacket)?;
    let end = header.bytes.len();
    let offset = header.transport_offset;
    let plan = l4_plan(from.l4, to.l4, &packet[offset..end], false, false)?;
    if from == to {
        return Ok(());
    }
    apply_l4(
        &mut packet[offset..end],
        plan,
        old_src,
        old_dst,
        new_src,
        new_dst,
    );
    packet[8..24].copy_from_slice(&new_src);
    packet[24..40].copy_from_slice(&new_dst);
    Ok(())
}

/// Rewrite the error's quoted packet first, then its enclosing ICMPv6
/// datagram. A source NAT operation on the outer error corresponds to a
/// destination operation on the quote, and conversely.
pub(crate) fn rewrite_ipv6_related_icmp(
    packet: &mut [u8],
    quoted_from: CtTuple,
    quoted_to: CtTuple,
    side: NatManipSide,
    mode: CtChecksumMode,
) -> Result<(), NatRewriteError> {
    rewrite_ipv6_related_icmp_impl(packet, quoted_from, quoted_to, side, mode, false)
}

pub(crate) fn rewrite_ipv6_generated_related_icmp(
    packet: &mut [u8],
    quoted_from: CtTuple,
    quoted_to: CtTuple,
    side: NatManipSide,
    mode: CtChecksumMode,
) -> Result<(), NatRewriteError> {
    rewrite_ipv6_related_icmp_impl(packet, quoted_from, quoted_to, side, mode, true)
}

fn rewrite_ipv6_related_icmp_impl(
    packet: &mut [u8],
    quoted_from: CtTuple,
    quoted_to: CtTuple,
    side: NatManipSide,
    mode: CtChecksumMode,
    generated: bool,
) -> Result<(), NatRewriteError> {
    let Ipv6AddressRewrite {
        old_src,
        old_dst,
        new_src,
        new_dst,
    } = addresses(quoted_from, quoted_to)?;
    match side {
        NatManipSide::Destination if old_dst != new_dst => {
            return Err(NatRewriteError::Unsupported);
        }
        NatManipSide::Source if old_src != new_src => return Err(NatRewriteError::Unsupported),
        _ => {}
    }
    if let (
        CtL4::Tcp {
            src_port: a,
            dst_port: b,
        }
        | CtL4::Udp {
            src_port: a,
            dst_port: b,
        },
        CtL4::Tcp {
            src_port: c,
            dst_port: d,
        }
        | CtL4::Udp {
            src_port: c,
            dst_port: d,
        },
    ) = (quoted_from.l4, quoted_to.l4)
    {
        if (side == NatManipSide::Destination && b != d) || (side == NatManipSide::Source && a != c)
        {
            return Err(NatRewriteError::Unsupported);
        }
    }
    if !generated {
        let ParsedCtPacket::Related {
            quoted,
            outer_destination,
        } = parse_ipv6_conntrack_with_mode(packet, mode)
        else {
            return Err(NatRewriteError::InvalidPacket);
        };
        if quoted != quoted_from
            || (side == NatManipSide::Destination && outer_destination != quoted_from.src)
        {
            return Err(NatRewriteError::TupleMismatch);
        }
    }
    let outer = ipv6_header(packet, false).ok_or(NatRewriteError::InvalidPacket)?;
    if outer.protocol != 58 {
        return Err(NatRewriteError::InvalidPacket);
    }
    let outer_end = outer.bytes.len();
    let icmp_offset = outer.transport_offset;
    if icmp_offset.checked_add(8).is_none_or(|end| end > outer_end) {
        return Err(NatRewriteError::InvalidPacket);
    }
    let quote_offset = icmp_offset + if packet[icmp_offset] == 137 { 48 } else { 8 };
    if packet[icmp_offset] == 137 && quoted_from != quoted_to {
        // Linux rejects a redirect whose quoted route advice needs NAT.
        return Err(NatRewriteError::Unsupported);
    }
    if quote_offset + 40 > outer_end {
        return Err(NatRewriteError::InvalidPacket);
    }
    let inner = ipv6_header(&packet[quote_offset..outer_end], true);
    if inner.is_none() && !generated {
        return Err(NatRewriteError::InvalidPacket);
    }
    if generated
        && (packet[quote_offset + 8..quote_offset + 24] != old_src
            || packet[quote_offset + 24..quote_offset + 40] != old_dst
            || (side == NatManipSide::Destination && packet[24..40] != old_src))
    {
        return Err(NatRewriteError::TupleMismatch);
    }
    let (l4_offset, l4_end) = inner.as_ref().map_or((outer_end, outer_end), |inner| {
        (
            quote_offset + inner.transport_offset,
            quote_offset + inner.bytes.len(),
        )
    });
    if generated {
        if let Some(inner) = inner.as_ref() {
            let expected_protocol = match quoted_from.l4 {
                CtL4::Tcp { .. } => 6,
                CtL4::Udp { .. } => 17,
                CtL4::Icmpv6 { .. } => 58,
                CtL4::Generic { protocol } => protocol,
                CtL4::Icmp { .. } => return Err(NatRewriteError::Unsupported),
            };
            if inner.protocol != expected_protocol {
                return Err(NatRewriteError::TupleMismatch);
            }
            let l4 = &packet[l4_offset..l4_end];
            let (prefix, prefix_len) = match quoted_from.l4 {
                CtL4::Tcp { src_port, dst_port } | CtL4::Udp { src_port, dst_port } => {
                    let a = src_port.to_be_bytes();
                    let b = dst_port.to_be_bytes();
                    ([a[0], a[1], b[0], b[1]], 4)
                }
                CtL4::Icmpv6 { kind, code, .. } => ([kind, code, 0, 0], 2),
                CtL4::Generic { .. } => ([0; 4], 0),
                CtL4::Icmp { .. } => unreachable!(),
            };
            let available = l4.len().min(prefix_len);
            if l4[..available] != prefix[..available] {
                return Err(NatRewriteError::TupleMismatch);
            }
        }
    }
    let plan = l4_plan(
        quoted_from.l4,
        quoted_to.l4,
        &packet[l4_offset..l4_end],
        true,
        generated,
    )?;
    if quoted_from == quoted_to {
        return Ok(());
    }
    let (translated_src, translated_dst) = match side {
        NatManipSide::Destination => (outer.source, new_src),
        NatManipSide::Source => (new_dst, outer.destination),
    };
    apply_l4(
        &mut packet[l4_offset..l4_end],
        plan,
        old_src,
        old_dst,
        new_src,
        new_dst,
    );
    packet[quote_offset + 8..quote_offset + 24].copy_from_slice(&new_src);
    packet[quote_offset + 24..quote_offset + 40].copy_from_slice(&new_dst);
    match side {
        NatManipSide::Destination => packet[24..40].copy_from_slice(&translated_dst),
        NatManipSide::Source => packet[8..24].copy_from_slice(&translated_src),
    }
    // The quote can have a partial TCP header; recomputing the enclosing
    // ICMPv6 checksum is necessary because the changed quote itself is data.
    packet[icmp_offset + 2..icmp_offset + 4].fill(0);
    let next = checksum_ipv6(
        &packet[icmp_offset..outer_end],
        translated_src,
        translated_dst,
        58,
    );
    packet[icmp_offset + 2..icmp_offset + 4].copy_from_slice(&next.to_be_bytes());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    const A: [u8; 16] = [0xfd, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1];
    const B: [u8; 16] = {
        let mut bytes = A;
        bytes[15] = 2;
        bytes
    };
    const C: [u8; 16] = {
        let mut bytes = A;
        bytes[15] = 3;
        bytes
    };
    const R: [u8; 16] = [0xfd, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 4];

    fn tuple(src: [u8; 16], dst: [u8; 16], l4: CtL4) -> CtTuple {
        CtTuple {
            src: src.into(),
            dst: dst.into(),
            l4,
        }
    }

    fn ip(src: [u8; 16], dst: [u8; 16], next: u8, body: &[u8]) -> Vec<u8> {
        let mut packet = Vec::from([0u8; 40]);
        packet[0] = 0x60;
        packet[4..6].copy_from_slice(&(body.len() as u16).to_be_bytes());
        packet[6] = next;
        packet[7] = 64;
        packet[8..24].copy_from_slice(&src);
        packet[24..40].copy_from_slice(&dst);
        packet.extend_from_slice(body);
        packet
    }

    fn fill_checksum(body: &mut [u8], offset: usize, src: [u8; 16], dst: [u8; 16], proto: u8) {
        body[offset..offset + 2].fill(0);
        let value = checksum_ipv6(body, src, dst, proto);
        body[offset..offset + 2].copy_from_slice(&value.to_be_bytes());
    }

    fn udp(src: [u8; 16], dst: [u8; 16], sport: u16, dport: u16) -> Vec<u8> {
        let mut body = Vec::from([0u8; 12]);
        body[0..2].copy_from_slice(&sport.to_be_bytes());
        body[2..4].copy_from_slice(&dport.to_be_bytes());
        body[4..6].copy_from_slice(&12u16.to_be_bytes());
        body[8..12].copy_from_slice(b"data");
        fill_checksum(&mut body, 6, src, dst, 17);
        body
    }

    fn tcp(src: [u8; 16], dst: [u8; 16], sport: u16, dport: u16) -> Vec<u8> {
        let mut body = Vec::from([0u8; 20]);
        body[0..2].copy_from_slice(&sport.to_be_bytes());
        body[2..4].copy_from_slice(&dport.to_be_bytes());
        body[4..8].copy_from_slice(&1234u32.to_be_bytes());
        body[12] = 5 << 4;
        body[13] = 2;
        body[14..16].copy_from_slice(&4096u16.to_be_bytes());
        fill_checksum(&mut body, 16, src, dst, 6);
        body
    }

    fn icmp(src: [u8; 16], dst: [u8; 16], kind: u8, id: u16) -> Vec<u8> {
        let mut body = Vec::from([0u8; 8]);
        body[0] = kind;
        body[4..6].copy_from_slice(&id.to_be_bytes());
        fill_checksum(&mut body, 2, src, dst, 58);
        body
    }

    fn error(src: [u8; 16], dst: [u8; 16], quote: &[u8]) -> Vec<u8> {
        let mut body = Vec::from([0u8; 8]);
        body[0] = 1;
        body.extend_from_slice(quote);
        fill_checksum(&mut body, 2, src, dst, 58);
        ip(src, dst, 58, &body)
    }

    fn verified(packet: &[u8], protocol: u8, offset: usize) -> bool {
        let src: [u8; 16] = packet[8..24].try_into().unwrap();
        let dst: [u8; 16] = packet[24..40].try_into().unwrap();
        checksum_ipv6(&packet[offset..], src, dst, protocol) == 0
    }

    #[test]
    fn udp_and_tcp_tuple_rewrite_preserve_transport_checksums() {
        let old_udp = tuple(
            A,
            B,
            CtL4::Udp {
                src_port: 1234,
                dst_port: 80,
            },
        );
        let new_udp = tuple(
            C,
            R,
            CtL4::Udp {
                src_port: 4321,
                dst_port: 8080,
            },
        );
        let mut packet = ip(A, B, 17, &udp(A, B, 1234, 80));
        rewrite_ipv6_tuple(&mut packet, old_udp, new_udp, CtChecksumMode::Verify).unwrap();
        assert_eq!(&packet[8..24], &C);
        assert_eq!(&packet[24..40], &R);
        assert_eq!(&packet[40..44], &[0x10, 0xe1, 0x1f, 0x90]);
        assert!(verified(&packet, 17, 40));

        let old_tcp = tuple(
            A,
            B,
            CtL4::Tcp {
                src_port: 1234,
                dst_port: 80,
            },
        );
        let new_tcp = tuple(
            C,
            B,
            CtL4::Tcp {
                src_port: 4321,
                dst_port: 80,
            },
        );
        let mut packet = ip(A, B, 6, &tcp(A, B, 1234, 80));
        rewrite_ipv6_tuple(&mut packet, old_tcp, new_tcp, CtChecksumMode::Verify).unwrap();
        assert_eq!(&packet[8..24], &C);
        assert!(verified(&packet, 6, 40));
    }

    #[test]
    fn extension_header_and_echo_identifier_are_rewritten() {
        let old = tuple(
            A,
            B,
            CtL4::Icmpv6 {
                identifier: 7,
                kind: 128,
                code: 0,
            },
        );
        let new = tuple(
            C,
            B,
            CtL4::Icmpv6 {
                identifier: 9,
                kind: 128,
                code: 0,
            },
        );
        let mut body = Vec::from([0u8; 8]);
        body[0] = 58;
        body.extend_from_slice(&icmp(A, B, 128, 7));
        let mut packet = ip(A, B, 0, &body);
        rewrite_ipv6_tuple(&mut packet, old, new, CtChecksumMode::Verify).unwrap();
        assert_eq!(&packet[52..54], &9u16.to_be_bytes());
        assert!(verified(&packet, 58, 48));
    }

    #[test]
    fn udp_zero_checksum_is_preserved_like_linux_nat() {
        let old = tuple(
            A,
            B,
            CtL4::Udp {
                src_port: 1234,
                dst_port: 80,
            },
        );
        let new = tuple(
            C,
            B,
            CtL4::Udp {
                src_port: 4321,
                dst_port: 80,
            },
        );
        let mut packet = ip(A, B, 17, &udp(A, B, 1234, 80));
        packet[46..48].fill(0);
        rewrite_ipv6_tuple(&mut packet, old, new, CtChecksumMode::Verify).unwrap();
        assert_eq!(&packet[46..48], &[0, 0]);
        assert_eq!(&packet[40..42], &4321u16.to_be_bytes());
    }

    #[test]
    fn related_short_tcp_quote_rewrites_outer_and_inner_without_tcp_checksum() {
        let old = tuple(
            A,
            B,
            CtL4::Tcp {
                src_port: 1234,
                dst_port: 80,
            },
        );
        let new = tuple(
            C,
            B,
            CtL4::Tcp {
                src_port: 4321,
                dst_port: 80,
            },
        );
        let quoted = ip(A, B, 6, &tcp(A, B, 1234, 80)[..8]);
        let mut packet = error(R, A, &quoted);
        rewrite_ipv6_related_icmp(
            &mut packet,
            old,
            new,
            NatManipSide::Destination,
            CtChecksumMode::Verify,
        )
        .unwrap();
        assert_eq!(&packet[24..40], &C);
        assert_eq!(&packet[56..72], &C);
        assert_eq!(&packet[88..90], &4321u16.to_be_bytes());
        assert!(verified(&packet, 58, 40));
    }

    #[test]
    fn related_full_udp_quote_updates_both_checksums() {
        let old = tuple(
            A,
            B,
            CtL4::Udp {
                src_port: 1234,
                dst_port: 80,
            },
        );
        let new = tuple(
            C,
            B,
            CtL4::Udp {
                src_port: 4321,
                dst_port: 80,
            },
        );
        let quoted = ip(A, B, 17, &udp(A, B, 1234, 80));
        let mut packet = error(R, A, &quoted);
        rewrite_ipv6_related_icmp(
            &mut packet,
            old,
            new,
            NatManipSide::Destination,
            CtChecksumMode::Verify,
        )
        .unwrap();
        assert!(verified(&packet, 58, 40));
        let inner = &packet[48..];
        assert_eq!(&inner[8..24], &C);
        assert!(verified(inner, 17, 40));
    }

    #[test]
    fn generated_related_keeps_header_only_ipv6_quote_valid() {
        let from = tuple(
            A,
            B,
            CtL4::Udp {
                src_port: 1234,
                dst_port: 80,
            },
        );
        let to = tuple(
            C,
            B,
            CtL4::Udp {
                src_port: 4321,
                dst_port: 80,
            },
        );
        let quoted = ip(A, B, 17, &[]);
        let mut packet = error(R, A, &quoted);
        assert_eq!(
            rewrite_ipv6_related_icmp(
                &mut packet,
                from,
                to,
                NatManipSide::Destination,
                CtChecksumMode::Skip,
            ),
            Err(NatRewriteError::InvalidPacket)
        );
        rewrite_ipv6_generated_related_icmp(
            &mut packet,
            from,
            to,
            NatManipSide::Destination,
            CtChecksumMode::Skip,
        )
        .unwrap();
        assert_eq!(&packet[24..40], &C);
        assert_eq!(&packet[56..72], &C);
        assert!(verified(&packet, 58, 40));
    }

    #[test]
    fn related_full_tcp_quote_adjusts_present_inner_checksum() {
        let old = tuple(
            A,
            B,
            CtL4::Tcp {
                src_port: 1234,
                dst_port: 80,
            },
        );
        let new = tuple(
            C,
            B,
            CtL4::Tcp {
                src_port: 4321,
                dst_port: 80,
            },
        );
        let quoted = ip(A, B, 6, &tcp(A, B, 1234, 80));
        let mut packet = error(R, A, &quoted);
        rewrite_ipv6_related_icmp(
            &mut packet,
            old,
            new,
            NatManipSide::Destination,
            CtChecksumMode::Verify,
        )
        .unwrap();
        assert!(verified(&packet, 58, 40));
        assert!(verified(&packet[48..], 6, 40));
    }

    #[test]
    fn related_short_tcp_quote_with_extension_headers_stays_in_bounds() {
        let old = tuple(
            A,
            B,
            CtL4::Tcp {
                src_port: 1234,
                dst_port: 80,
            },
        );
        let new = tuple(
            C,
            B,
            CtL4::Tcp {
                src_port: 4321,
                dst_port: 80,
            },
        );
        let mut inner_body = Vec::from([0u8; 8]);
        inner_body[0] = 6;
        inner_body.extend_from_slice(&tcp(A, B, 1234, 80)[..8]);
        let quoted = ip(A, B, 0, &inner_body);
        let mut outer_body = Vec::from([0u8; 8]);
        outer_body[0] = 58;
        let mut icmp_body = Vec::from([0u8; 8]);
        icmp_body[0] = 1;
        icmp_body.extend_from_slice(&quoted);
        fill_checksum(&mut icmp_body, 2, R, A, 58);
        outer_body.extend_from_slice(&icmp_body);
        let mut packet = ip(R, A, 0, &outer_body);
        rewrite_ipv6_related_icmp(
            &mut packet,
            old,
            new,
            NatManipSide::Destination,
            CtChecksumMode::Verify,
        )
        .unwrap();
        assert_eq!(&packet[24..40], &C);
        assert_eq!(&packet[64..80], &C);
        assert_eq!(&packet[104..106], &4321u16.to_be_bytes());
        assert_eq!(checksum_ipv6(&packet[48..], R, C, 58), 0);
    }

    #[test]
    fn related_source_side_rewrites_outer_source_and_quote_destination() {
        let old = tuple(
            A,
            B,
            CtL4::Icmpv6 {
                identifier: 7,
                kind: 128,
                code: 0,
            },
        );
        let new = tuple(
            A,
            C,
            CtL4::Icmpv6 {
                identifier: 9,
                kind: 128,
                code: 0,
            },
        );
        let quote = ip(A, B, 58, &icmp(A, B, 128, 7));
        let mut packet = error(R, A, &quote);
        rewrite_ipv6_related_icmp(
            &mut packet,
            old,
            new,
            NatManipSide::Source,
            CtChecksumMode::Verify,
        )
        .unwrap();
        assert_eq!(&packet[8..24], &C);
        assert_eq!(&packet[72..88], &C);
        assert_eq!(&packet[92..94], &9u16.to_be_bytes());
        assert!(verified(&packet, 58, 40));
        assert!(verified(&packet[48..], 58, 40));
    }

    #[test]
    fn mismatch_and_bad_checksum_never_partially_mutate() {
        let old = tuple(
            A,
            B,
            CtL4::Udp {
                src_port: 1234,
                dst_port: 80,
            },
        );
        let wrong = tuple(
            C,
            B,
            CtL4::Udp {
                src_port: 4321,
                dst_port: 80,
            },
        );
        let mut packet = ip(A, B, 17, &udp(A, B, 1234, 80));
        packet[51] ^= 1;
        let before = packet.clone();
        assert_eq!(
            rewrite_ipv6_tuple(&mut packet, old, wrong, CtChecksumMode::Verify),
            Err(NatRewriteError::InvalidPacket)
        );
        assert_eq!(packet, before);
        rewrite_ipv6_tuple(&mut packet, old, wrong, CtChecksumMode::Skip).unwrap();
        assert_eq!(&packet[8..24], &C);

        let quote = ip(A, B, 17, &udp(A, B, 1234, 80));
        let mut packet = error(R, A, &quote);
        let before = packet.clone();
        assert_eq!(
            rewrite_ipv6_related_icmp(
                &mut packet,
                old,
                wrong,
                NatManipSide::Source,
                CtChecksumMode::Verify
            ),
            Err(NatRewriteError::Unsupported)
        );
        assert_eq!(packet, before);

        let mut packet = error(R, A, &quote);
        packet[42] ^= 1;
        let before = packet.clone();
        assert_eq!(
            rewrite_ipv6_related_icmp(
                &mut packet,
                old,
                wrong,
                NatManipSide::Destination,
                CtChecksumMode::Verify
            ),
            Err(NatRewriteError::InvalidPacket)
        );
        assert_eq!(packet, before);
    }

    #[test]
    fn generic_ipv6_rewrites_only_ip_addresses() {
        let from = CtTuple {
            src: A.into(),
            dst: B.into(),
            l4: CtL4::Generic { protocol: 47 },
        };
        let to = CtTuple {
            src: R.into(),
            dst: A.into(),
            ..from
        };
        let mut packet = Vec::from([0u8; 44]);
        packet[0] = 0x60;
        packet[4..6].copy_from_slice(&4u16.to_be_bytes());
        packet[6] = 47;
        packet[7] = 64;
        packet[8..24].copy_from_slice(&A);
        packet[24..40].copy_from_slice(&B);
        packet[40..44].copy_from_slice(b"GRE!");
        rewrite_ipv6_tuple(&mut packet, from, to, CtChecksumMode::Verify).unwrap();
        assert_eq!(&packet[8..24], &R);
        assert_eq!(&packet[24..40], &A);
        assert_eq!(&packet[40..], b"GRE!");
    }
}
