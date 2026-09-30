//! In-place IPv4 NAT tuple rewrite and one's-complement checksum adjustment.
//!
//! Callers own CT lookup and hook ordering. This module only mutates a
//! complete, validated datagram matching `from`; it never chooses a mapping.

use super::{
    packet::{
        checksum, ipv4_header, parse_ipv4_conntrack_with_mode, CtChecksumMode, Ipv4Header,
        ParsedCtPacket,
    },
    CtAddress, CtL4, CtTuple,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum NatRewriteError {
    InvalidPacket,
    TupleMismatch,
    Unsupported,
}

/// The outer IPv4 field changed at this Netfilter hook. An ICMP error quotes
/// the packet traveling in the opposite direction, so its inner field is the
/// opposite side: destination NAT rewrites the quote's source and source NAT
/// rewrites the quote's destination.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum NatManipSide {
    Destination,
    Source,
}

fn copy_available_prefix(destination: &mut [u8], replacement: &[u8]) {
    let len = destination.len().min(replacement.len());
    destination[..len].copy_from_slice(&replacement[..len]);
}

/// A redirect contains next-hop advice in the pre-NAT address space. Linux
/// drops it for a NATed flow even at a hook where this particular side is a
/// null binding.
pub(crate) fn is_ipv4_related_redirect(packet: &[u8], mode: CtChecksumMode) -> bool {
    if !matches!(
        parse_ipv4_conntrack_with_mode(packet, mode),
        ParsedCtPacket::Related { .. }
    ) {
        return false;
    }
    ipv4_header(packet, true)
        .is_some_and(|header| header.protocol == 1 && packet.get(header.header_len) == Some(&5))
}

/// Translate a RELATED IPv4 ICMP error and the offending packet quoted in it.
/// `quoted_from` is the quote *at this hook*, not necessarily the original
/// on-wire tuple: a prior hook may already have translated its other side.
/// The caller derives `quoted_to` from the matched flow and NAT direction.
///
/// All bounds, tuple, family and side checks precede the first write. The
/// quote can contain only eight TCP bytes, so its TCP checksum field is
/// adjusted only when a complete TCP header is actually present. This mirrors
/// Linux 6.6's nf_nat_icmp_reply_translation()/tcp_manip_pkt().
/// The caller must reject ICMP Redirect for a flow NATed at either hook even
/// if this particular side is unchanged: that decision needs whole-flow NAT
/// state, not just `quoted_from` and `quoted_to` at one hook.
pub(crate) fn rewrite_ipv4_related_icmp(
    packet: &mut [u8],
    quoted_from: CtTuple,
    quoted_to: CtTuple,
    side: NatManipSide,
    checksum_mode: CtChecksumMode,
) -> Result<(), NatRewriteError> {
    rewrite_ipv4_related_icmp_impl(packet, quoted_from, quoted_to, side, checksum_mode, false)
}

pub(crate) fn rewrite_ipv4_generated_related_icmp(
    packet: &mut [u8],
    quoted_from: CtTuple,
    quoted_to: CtTuple,
    side: NatManipSide,
    checksum_mode: CtChecksumMode,
) -> Result<(), NatRewriteError> {
    rewrite_ipv4_related_icmp_impl(packet, quoted_from, quoted_to, side, checksum_mode, true)
}

fn rewrite_ipv4_related_icmp_impl(
    packet: &mut [u8],
    quoted_from: CtTuple,
    quoted_to: CtTuple,
    side: NatManipSide,
    checksum_mode: CtChecksumMode,
    generated: bool,
) -> Result<(), NatRewriteError> {
    let (
        CtAddress::V4(old_src),
        CtAddress::V4(old_dst),
        CtAddress::V4(new_src),
        CtAddress::V4(new_dst),
    ) = (
        quoted_from.src,
        quoted_from.dst,
        quoted_to.src,
        quoted_to.dst,
    )
    else {
        return Err(NatRewriteError::Unsupported);
    };
    let (old_src_port, old_dst_port, new_src_port, new_dst_port, icmp_identifier) =
        match (quoted_from.l4, quoted_to.l4) {
            (
                CtL4::Tcp {
                    src_port: old_src_port,
                    dst_port: old_dst_port,
                },
                CtL4::Tcp {
                    src_port: new_src_port,
                    dst_port: new_dst_port,
                },
            )
            | (
                CtL4::Udp {
                    src_port: old_src_port,
                    dst_port: old_dst_port,
                },
                CtL4::Udp {
                    src_port: new_src_port,
                    dst_port: new_dst_port,
                },
            ) => (
                old_src_port,
                old_dst_port,
                new_src_port,
                new_dst_port,
                false,
            ),
            (
                CtL4::Icmp {
                    identifier: old_id,
                    kind: old_kind,
                    code: old_code,
                },
                CtL4::Icmp {
                    identifier: new_id,
                    kind: new_kind,
                    code: new_code,
                },
            ) if old_kind == new_kind && old_code == new_code => (old_id, 0, new_id, 0, true),
            (CtL4::Generic { protocol: old }, CtL4::Generic { protocol: new }) if old == new => {
                (0, 0, 0, 0, false)
            }
            _ => return Err(NatRewriteError::Unsupported),
        };
    match side {
        NatManipSide::Destination
            if old_dst != new_dst || (!icmp_identifier && old_dst_port != new_dst_port) =>
        {
            return Err(NatRewriteError::Unsupported);
        }
        NatManipSide::Source
            if old_src != new_src || (!icmp_identifier && old_src_port != new_src_port) =>
        {
            return Err(NatRewriteError::Unsupported);
        }
        _ => {}
    }
    if !generated {
        let ParsedCtPacket::Related {
            quoted,
            outer_destination,
        } = parse_ipv4_conntrack_with_mode(packet, checksum_mode)
        else {
            return Err(NatRewriteError::InvalidPacket);
        };
        if quoted != quoted_from
            || (side == NatManipSide::Destination && outer_destination != quoted_from.src)
        {
            return Err(NatRewriteError::TupleMismatch);
        }
    }
    let Ipv4Header {
        header_len: outer_header_len,
        total_len,
        protocol,
        ..
    } = ipv4_header(packet, true).ok_or(NatRewriteError::InvalidPacket)?;
    if protocol != 1 {
        return Err(NatRewriteError::InvalidPacket);
    }
    if total_len < outer_header_len + 28 {
        return Err(NatRewriteError::InvalidPacket);
    }
    let icmp = &packet[outer_header_len..total_len];
    // Redirects that actually need NAT are invalid in Linux: a redirect's
    // next-hop advice would refer to an address outside the recipient's view.
    if icmp[0] == 5 && quoted_from != quoted_to {
        return Err(NatRewriteError::Unsupported);
    }
    let inner_offset = outer_header_len + 8;
    let quoted_bytes = &packet[inner_offset..total_len];
    let inner = ipv4_header(quoted_bytes, false);
    let (inner_header_len, inner_source, inner_destination, inner_protocol) =
        if let Some(inner) = inner {
            (
                inner.header_len,
                inner.source,
                inner.destination,
                inner.protocol,
            )
        } else if generated && quoted_bytes.len() >= 20 && quoted_bytes[0] >> 4 == 4 {
            // A minimum return MTU can truncate IPv4 options. The trigger
            // was validated before quoting; its first 20 bytes still carry
            // addresses, protocol, IHL and the incremental header checksum.
            let header_len = usize::from(quoted_bytes[0] & 0x0f) * 4;
            let full_len = usize::from(u16::from_be_bytes([quoted_bytes[2], quoted_bytes[3]]));
            if !(20..=60).contains(&header_len)
                || header_len <= quoted_bytes.len()
                || full_len < header_len
            {
                return Err(NatRewriteError::InvalidPacket);
            }
            (
                header_len,
                quoted_bytes[12..16]
                    .try_into()
                    .map_err(|_| NatRewriteError::InvalidPacket)?,
                quoted_bytes[16..20]
                    .try_into()
                    .map_err(|_| NatRewriteError::InvalidPacket)?,
                quoted_bytes[9],
            )
        } else {
            return Err(NatRewriteError::InvalidPacket);
        };
    let transport_offset = inner_offset + inner_header_len;
    // The parser checked IHL and each protocol's minimum quote length. Keep
    // a local bounds guard before indexing a quoted transport header.
    let required_l4 = if matches!(quoted_from.l4, CtL4::Generic { .. }) {
        0
    } else {
        8
    };
    if transport_offset
        .checked_add(required_l4)
        .is_none_or(|end| end > total_len && !generated)
    {
        return Err(NatRewriteError::InvalidPacket);
    }
    if generated {
        let expected_protocol = match quoted_from.l4 {
            CtL4::Tcp { .. } => 6,
            CtL4::Udp { .. } => 17,
            CtL4::Icmp { .. } => 1,
            CtL4::Generic { protocol } => protocol,
            CtL4::Icmpv6 { .. } => return Err(NatRewriteError::Unsupported),
        };
        if inner_source != old_src
            || inner_destination != old_dst
            || inner_protocol != expected_protocol
            || (side == NatManipSide::Destination && packet[16..20] != old_src)
        {
            return Err(NatRewriteError::TupleMismatch);
        }
        let actual = &packet[transport_offset.min(total_len)..total_len];
        let (expected_prefix, prefix_len) = match quoted_from.l4 {
            CtL4::Tcp { src_port, dst_port } | CtL4::Udp { src_port, dst_port } => {
                let a = src_port.to_be_bytes();
                let b = dst_port.to_be_bytes();
                ([a[0], a[1], b[0], b[1]], 4)
            }
            CtL4::Icmp { kind, code, .. } => ([kind, code, 0, 0], 2),
            CtL4::Generic { .. } => ([0; 4], 0),
            CtL4::Icmpv6 { .. } => unreachable!(),
        };
        let available = actual.len().min(prefix_len);
        if actual[..available] != expected_prefix[..available] {
            return Err(NatRewriteError::TupleMismatch);
        }
    }
    if quoted_from == quoted_to {
        return Ok(());
    }

    let (inner_old, inner_new, inner_addr_offset, outer_new, outer_addr_offset) = match side {
        NatManipSide::Destination => (old_src, new_src, inner_offset + 12, new_src, 16),
        NatManipSide::Source => (old_dst, new_dst, inner_offset + 16, new_dst, 12),
    };
    let outer_old: [u8; 4] = packet
        .get(outer_addr_offset..outer_addr_offset + 4)
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or(NatRewriteError::InvalidPacket)?;
    let l4 = &mut packet[transport_offset.min(total_len)..total_len];
    match (quoted_from.l4, quoted_to.l4) {
        (CtL4::Tcp { .. }, CtL4::Tcp { .. }) => {
            // The first eight quoted bytes carry ports, not the TCP checksum
            // at offset 16. Do not read or synthesize a missing field.
            if l4.len() >= 20 {
                adjust_transport_checksum(l4, 16, quoted_from, quoted_to)?;
            }
            let src = new_src_port.to_be_bytes();
            let dst = new_dst_port.to_be_bytes();
            let replacement = [src[0], src[1], dst[0], dst[1]];
            copy_available_prefix(l4, &replacement);
        }
        (CtL4::Udp { .. }, CtL4::Udp { .. }) => {
            if l4.len() >= 8 && (l4[6] != 0 || l4[7] != 0) {
                adjust_transport_checksum(l4, 6, quoted_from, quoted_to)?;
                if l4[6] == 0 && l4[7] == 0 {
                    l4[6..8].copy_from_slice(&u16::MAX.to_be_bytes());
                }
            }
            let src = new_src_port.to_be_bytes();
            let dst = new_dst_port.to_be_bytes();
            let replacement = [src[0], src[1], dst[0], dst[1]];
            copy_available_prefix(l4, &replacement);
        }
        (CtL4::Icmp { .. }, CtL4::Icmp { .. }) => {
            if l4.len() >= 6 {
                let prior = u16::from_be_bytes([l4[2], l4[3]]);
                l4[2..4].copy_from_slice(
                    &replace_word(prior, old_src_port, new_src_port).to_be_bytes(),
                );
                l4[4..6].copy_from_slice(&new_src_port.to_be_bytes());
            }
        }
        (CtL4::Generic { .. }, CtL4::Generic { .. }) => {}
        _ => unreachable!("validated quoted protocol"),
    }
    replace_ipv4_address(
        packet,
        inner_addr_offset,
        inner_old,
        inner_new,
        inner_offset + 10,
    );
    replace_ipv4_address(packet, outer_addr_offset, outer_old, outer_new, 10);
    packet[outer_header_len + 2..outer_header_len + 4].fill(0);
    let icmp_checksum = !checksum(&packet[outer_header_len..total_len], 0);
    packet[outer_header_len + 2..outer_header_len + 4]
        .copy_from_slice(&icmp_checksum.to_be_bytes());
    Ok(())
}

fn replace_ipv4_address(
    packet: &mut [u8],
    address_offset: usize,
    old: [u8; 4],
    new: [u8; 4],
    checksum_offset: usize,
) {
    let mut value = u16::from_be_bytes([packet[checksum_offset], packet[checksum_offset + 1]]);
    for index in [0, 2] {
        value = replace_word(
            value,
            u16::from_be_bytes([old[index], old[index + 1]]),
            u16::from_be_bytes([new[index], new[index + 1]]),
        );
    }
    packet[checksum_offset..checksum_offset + 2].copy_from_slice(&value.to_be_bytes());
    packet[address_offset..address_offset + 4].copy_from_slice(&new);
}

/// `Skip` is for LOCAL_OUT packets whose transport checksum may be represented
/// by output metadata rather than yet verified by an ingress device. It does
/// not make a malformed checksum correct; callers must preserve or finalize
/// the output checksum contract before emission.
pub(crate) fn rewrite_ipv4_tuple(
    packet: &mut [u8],
    from: CtTuple,
    to: CtTuple,
    checksum_mode: CtChecksumMode,
) -> Result<(), NatRewriteError> {
    let (CtAddress::V4(_), CtAddress::V4(_), CtAddress::V4(new_src), CtAddress::V4(new_dst)) =
        (from.src, from.dst, to.src, to.dst)
    else {
        return Err(NatRewriteError::Unsupported);
    };
    if core::mem::discriminant(&from.l4) != core::mem::discriminant(&to.l4) {
        return Err(NatRewriteError::Unsupported);
    }
    let ParsedCtPacket::Flow { tuple, .. } = parse_ipv4_conntrack_with_mode(packet, checksum_mode)
    else {
        return Err(NatRewriteError::InvalidPacket);
    };
    if tuple != from {
        return Err(NatRewriteError::TupleMismatch);
    }
    let Ipv4Header {
        header_len,
        total_len,
        ..
    } = ipv4_header(packet, true).ok_or(NatRewriteError::InvalidPacket)?;
    let l4 = &mut packet[header_len..total_len];
    match (from.l4, to.l4) {
        (
            CtL4::Tcp { .. },
            CtL4::Tcp {
                src_port: new_sport,
                dst_port: new_dport,
            },
        ) => {
            adjust_transport_checksum(l4, 16, from, to)?;
            l4[0..2].copy_from_slice(&new_sport.to_be_bytes());
            l4[2..4].copy_from_slice(&new_dport.to_be_bytes());
        }
        (
            CtL4::Udp { .. },
            CtL4::Udp {
                src_port: new_sport,
                dst_port: new_dport,
            },
        ) => {
            if l4[6] != 0 || l4[7] != 0 {
                adjust_transport_checksum(l4, 6, from, to)?;
                if l4[6] == 0 && l4[7] == 0 {
                    l4[6..8].copy_from_slice(&u16::MAX.to_be_bytes());
                }
            }
            l4[0..2].copy_from_slice(&new_sport.to_be_bytes());
            l4[2..4].copy_from_slice(&new_dport.to_be_bytes());
        }
        (
            CtL4::Icmp {
                identifier: old_id,
                kind: old_kind,
                code: old_code,
            },
            CtL4::Icmp {
                identifier: new_id,
                kind: new_kind,
                code: new_code,
            },
        ) if old_kind == new_kind && old_code == new_code => {
            let prior = u16::from_be_bytes([l4[2], l4[3]]);
            l4[2..4].copy_from_slice(&replace_word(prior, old_id, new_id).to_be_bytes());
            l4[4..6].copy_from_slice(&new_id.to_be_bytes());
        }
        (CtL4::Generic { protocol: old }, CtL4::Generic { protocol: new }) if old == new => {}
        _ => return Err(NatRewriteError::Unsupported),
    }
    packet[12..16].copy_from_slice(&new_src);
    packet[16..20].copy_from_slice(&new_dst);
    packet[10..12].fill(0);
    let checksum = !checksum(&packet[..header_len], 0);
    packet[10..12].copy_from_slice(&checksum.to_be_bytes());
    Ok(())
}

fn adjust_transport_checksum(
    l4: &mut [u8],
    checksum_offset: usize,
    from: CtTuple,
    to: CtTuple,
) -> Result<(), NatRewriteError> {
    let (
        CtAddress::V4(old_src),
        CtAddress::V4(old_dst),
        CtAddress::V4(new_src),
        CtAddress::V4(new_dst),
    ) = (from.src, from.dst, to.src, to.dst)
    else {
        return Err(NatRewriteError::Unsupported);
    };
    let (old_sport, old_dport, new_sport, new_dport) = match (from.l4, to.l4) {
        (
            CtL4::Tcp {
                src_port: old_sport,
                dst_port: old_dport,
            },
            CtL4::Tcp {
                src_port: new_sport,
                dst_port: new_dport,
            },
        )
        | (
            CtL4::Udp {
                src_port: old_sport,
                dst_port: old_dport,
            },
            CtL4::Udp {
                src_port: new_sport,
                dst_port: new_dport,
            },
        ) => (old_sport, old_dport, new_sport, new_dport),
        _ => return Err(NatRewriteError::Unsupported),
    };
    let mut value = u16::from_be_bytes([l4[checksum_offset], l4[checksum_offset + 1]]);
    for (old, new) in [
        (
            u16::from_be_bytes([old_src[0], old_src[1]]),
            u16::from_be_bytes([new_src[0], new_src[1]]),
        ),
        (
            u16::from_be_bytes([old_src[2], old_src[3]]),
            u16::from_be_bytes([new_src[2], new_src[3]]),
        ),
        (
            u16::from_be_bytes([old_dst[0], old_dst[1]]),
            u16::from_be_bytes([new_dst[0], new_dst[1]]),
        ),
        (
            u16::from_be_bytes([old_dst[2], old_dst[3]]),
            u16::from_be_bytes([new_dst[2], new_dst[3]]),
        ),
        (old_sport, new_sport),
        (old_dport, new_dport),
    ] {
        value = replace_word(value, old, new);
    }
    l4[checksum_offset..checksum_offset + 2].copy_from_slice(&value.to_be_bytes());
    Ok(())
}

/// RFC 1624 HC' = ~(~HC + ~m + m'), using explicit end-around carries.
fn replace_word(checksum: u16, old: u16, new: u16) -> u16 {
    let mut sum = u32::from(!checksum) + u32::from(!old) + u32::from(new);
    sum = (sum & 0xffff) + (sum >> 16);
    sum = (sum & 0xffff) + (sum >> 16);
    !(sum as u16)
}

#[cfg(test)]
mod tests {
    use super::super::packet::parse_ipv4_conntrack;
    use super::*;
    use alloc::vec::Vec;

    fn tuple(src: [u8; 4], dst: [u8; 4], sport: u16, dport: u16) -> CtTuple {
        CtTuple {
            src: src.into(),
            dst: dst.into(),
            l4: CtL4::Udp {
                src_port: sport,
                dst_port: dport,
            },
        }
    }

    fn udp_packet(from: CtTuple, with_checksum: bool) -> Vec<u8> {
        let (CtAddress::V4(src), CtAddress::V4(dst)) = (from.src, from.dst) else {
            unreachable!()
        };
        let CtL4::Udp { src_port, dst_port } = from.l4 else {
            unreachable!()
        };
        let mut bytes = Vec::from([0u8; 32]);
        bytes[0] = 0x45;
        bytes[2..4].copy_from_slice(&32u16.to_be_bytes());
        bytes[8] = 64;
        bytes[9] = 17;
        bytes[12..16].copy_from_slice(&src);
        bytes[16..20].copy_from_slice(&dst);
        bytes[20..22].copy_from_slice(&src_port.to_be_bytes());
        bytes[22..24].copy_from_slice(&dst_port.to_be_bytes());
        bytes[24..26].copy_from_slice(&12u16.to_be_bytes());
        bytes[28..32].copy_from_slice(b"data");
        if with_checksum {
            let initial = u32::from(u16::from_be_bytes([src[0], src[1]]))
                + u32::from(u16::from_be_bytes([src[2], src[3]]))
                + u32::from(u16::from_be_bytes([dst[0], dst[1]]))
                + u32::from(u16::from_be_bytes([dst[2], dst[3]]))
                + 17
                + 12;
            let sum = !checksum(&bytes[20..], initial);
            bytes[26..28].copy_from_slice(&sum.to_be_bytes());
        }
        let sum = !checksum(&bytes[..20], 0);
        bytes[10..12].copy_from_slice(&sum.to_be_bytes());
        bytes
    }

    fn tcp_packet(from: CtTuple) -> Vec<u8> {
        let (CtAddress::V4(src), CtAddress::V4(dst)) = (from.src, from.dst) else {
            unreachable!()
        };
        let CtL4::Tcp { src_port, dst_port } = from.l4 else {
            unreachable!()
        };
        let mut bytes = Vec::from([0u8; 40]);
        bytes[0] = 0x45;
        bytes[2..4].copy_from_slice(&40u16.to_be_bytes());
        bytes[8] = 64;
        bytes[9] = 6;
        bytes[12..16].copy_from_slice(&src);
        bytes[16..20].copy_from_slice(&dst);
        bytes[20..22].copy_from_slice(&src_port.to_be_bytes());
        bytes[22..24].copy_from_slice(&dst_port.to_be_bytes());
        bytes[24..28].copy_from_slice(&1u32.to_be_bytes());
        bytes[32] = 5 << 4;
        bytes[33] = 2;
        bytes[34..36].copy_from_slice(&4096u16.to_be_bytes());
        let initial = u32::from(u16::from_be_bytes([src[0], src[1]]))
            + u32::from(u16::from_be_bytes([src[2], src[3]]))
            + u32::from(u16::from_be_bytes([dst[0], dst[1]]))
            + u32::from(u16::from_be_bytes([dst[2], dst[3]]))
            + 6
            + 20;
        let tcp_checksum = !checksum(&bytes[20..], initial);
        bytes[36..38].copy_from_slice(&tcp_checksum.to_be_bytes());
        let ip_checksum = !checksum(&bytes[..20], 0);
        bytes[10..12].copy_from_slice(&ip_checksum.to_be_bytes());
        bytes
    }

    fn icmp_packet(from: CtTuple) -> Vec<u8> {
        let (CtAddress::V4(src), CtAddress::V4(dst)) = (from.src, from.dst) else {
            unreachable!()
        };
        let CtL4::Icmp {
            identifier,
            kind,
            code,
        } = from.l4
        else {
            unreachable!()
        };
        let mut bytes = Vec::from([0u8; 28]);
        bytes[0] = 0x45;
        bytes[2..4].copy_from_slice(&28u16.to_be_bytes());
        bytes[8] = 64;
        bytes[9] = 1;
        bytes[12..16].copy_from_slice(&src);
        bytes[16..20].copy_from_slice(&dst);
        bytes[20] = kind;
        bytes[21] = code;
        bytes[24..26].copy_from_slice(&identifier.to_be_bytes());
        let icmp_checksum = !checksum(&bytes[20..], 0);
        bytes[22..24].copy_from_slice(&icmp_checksum.to_be_bytes());
        let ip_checksum = !checksum(&bytes[..20], 0);
        bytes[10..12].copy_from_slice(&ip_checksum.to_be_bytes());
        bytes
    }

    fn icmp_error(quote: &[u8], source: [u8; 4], destination: [u8; 4]) -> Vec<u8> {
        let mut bytes = Vec::from([0u8; 28]);
        bytes.extend_from_slice(quote);
        bytes[0] = 0x45;
        let length = u16::try_from(bytes.len()).unwrap();
        bytes[2..4].copy_from_slice(&length.to_be_bytes());
        bytes[8] = 64;
        bytes[9] = 1;
        bytes[12..16].copy_from_slice(&source);
        bytes[16..20].copy_from_slice(&destination);
        bytes[20] = 3;
        bytes[21] = 3;
        let icmp_checksum = !checksum(&bytes[20..], 0);
        bytes[22..24].copy_from_slice(&icmp_checksum.to_be_bytes());
        let ip_checksum = !checksum(&bytes[..20], 0);
        bytes[10..12].copy_from_slice(&ip_checksum.to_be_bytes());
        bytes
    }

    fn assert_related_checksums(packet: &[u8], quoted: CtTuple) {
        assert_eq!(checksum(&packet[..20], 0), u16::MAX);
        assert_eq!(checksum(&packet[20..], 0), u16::MAX);
        assert_eq!(checksum(&packet[28..48], 0), u16::MAX);
        assert!(matches!(
            parse_ipv4_conntrack(packet),
            ParsedCtPacket::Related { quoted: actual, .. } if actual == quoted
        ));
    }

    #[test]
    fn udp_mapping_updates_both_checksums_and_tuple() {
        let from = tuple([10, 0, 0, 2], [192, 0, 2, 1], 1234, 80);
        let to = tuple([203, 0, 113, 9], [192, 0, 2, 3], 40000, 8080);
        for with_checksum in [false, true] {
            let mut packet = udp_packet(from, with_checksum);
            rewrite_ipv4_tuple(&mut packet, from, to, CtChecksumMode::Verify).unwrap();
            assert!(
                matches!(parse_ipv4_conntrack(&packet), ParsedCtPacket::Flow { tuple, .. } if tuple == to)
            );
            assert_eq!(packet[26..28] == [0, 0], !with_checksum);
        }
    }

    #[test]
    fn local_output_mode_skips_input_checksum_verification_only() {
        let from = tuple([10, 0, 0, 2], [192, 0, 2, 1], 1234, 80);
        let to = tuple([10, 0, 0, 2], [192, 0, 2, 3], 1234, 8080);
        let mut packet = udp_packet(from, true);
        packet[26] ^= 0x40;
        let original = packet.clone();
        assert_eq!(
            rewrite_ipv4_tuple(&mut packet, from, to, CtChecksumMode::Verify),
            Err(NatRewriteError::InvalidPacket)
        );
        assert_eq!(packet, original);
        rewrite_ipv4_tuple(&mut packet, from, to, CtChecksumMode::Skip).unwrap();
        assert!(matches!(
            parse_ipv4_conntrack_with_mode(&packet, CtChecksumMode::Skip),
            ParsedCtPacket::Flow { tuple, .. } if tuple == to
        ));
        // Skip is not a checksum repair operation: the deliberately bad
        // input checksum remains bad after the address/port delta update.
        assert_eq!(parse_ipv4_conntrack(&packet), ParsedCtPacket::Invalid);
    }

    #[test]
    fn mismatched_tuple_rejects_without_mutating_packet() {
        let from = tuple([10, 0, 0, 2], [192, 0, 2, 1], 1234, 80);
        let other = tuple([10, 0, 0, 3], [192, 0, 2, 1], 1234, 80);
        let mut packet = udp_packet(from, true);
        let original = packet.clone();
        assert_eq!(
            rewrite_ipv4_tuple(&mut packet, other, from, CtChecksumMode::Verify),
            Err(NatRewriteError::TupleMismatch)
        );
        assert_eq!(packet, original);
    }

    #[test]
    fn tcp_mapping_preserves_transport_checksum() {
        let from = CtTuple {
            src: [10, 0, 0, 2].into(),
            dst: [192, 0, 2, 1].into(),
            l4: CtL4::Tcp {
                src_port: 1234,
                dst_port: 443,
            },
        };
        let to = CtTuple {
            src: [203, 0, 113, 9].into(),
            dst: [192, 0, 2, 3].into(),
            l4: CtL4::Tcp {
                src_port: 40000,
                dst_port: 8443,
            },
        };
        let mut packet = tcp_packet(from);
        rewrite_ipv4_tuple(&mut packet, from, to, CtChecksumMode::Verify).unwrap();
        assert!(
            matches!(parse_ipv4_conntrack(&packet), ParsedCtPacket::Flow { tuple, .. } if tuple == to)
        );
    }

    #[test]
    fn icmp_identifier_mapping_preserves_checksum() {
        let from = CtTuple {
            src: [10, 0, 0, 2].into(),
            dst: [192, 0, 2, 1].into(),
            l4: CtL4::Icmp {
                identifier: 1234,
                kind: 8,
                code: 0,
            },
        };
        let to = CtTuple {
            src: [203, 0, 113, 9].into(),
            dst: [192, 0, 2, 3].into(),
            l4: CtL4::Icmp {
                identifier: 40000,
                kind: 8,
                code: 0,
            },
        };
        let mut packet = icmp_packet(from);
        rewrite_ipv4_tuple(&mut packet, from, to, CtChecksumMode::Verify).unwrap();
        assert!(
            matches!(parse_ipv4_conntrack(&packet), ParsedCtPacket::Flow { tuple, .. } if tuple == to)
        );
    }

    #[test]
    fn related_udp_error_translates_both_nat_sides_and_checksums() {
        let original = tuple([10, 0, 0, 2], [198, 51, 100, 8], 1234, 80);
        let wire = tuple([203, 0, 113, 9], [192, 0, 2, 3], 40000, 8080);
        let after_destination = tuple([10, 0, 0, 2], [192, 0, 2, 3], 1234, 8080);
        let mut packet = icmp_error(&udp_packet(wire, true), [192, 0, 2, 3], [203, 0, 113, 9]);
        rewrite_ipv4_related_icmp(
            &mut packet,
            wire,
            after_destination,
            NatManipSide::Destination,
            CtChecksumMode::Verify,
        )
        .unwrap();
        assert_related_checksums(&packet, after_destination);
        assert_eq!(&packet[16..20], &[10, 0, 0, 2]);

        rewrite_ipv4_related_icmp(
            &mut packet,
            after_destination,
            original,
            NatManipSide::Source,
            CtChecksumMode::Verify,
        )
        .unwrap();
        assert_related_checksums(&packet, original);
        assert_eq!(&packet[12..20], &[198, 51, 100, 8, 10, 0, 0, 2]);
        assert_eq!(&packet[28..], udp_packet(original, true));
    }

    #[test]
    fn generated_related_rewrites_header_only_quote_at_minimum_return_mtu() {
        let from = tuple([10, 0, 0, 2], [192, 0, 2, 3], 1234, 8080);
        let to = tuple([10, 0, 0, 2], [203, 0, 113, 1], 1234, 8080);
        let complete = udp_packet(from, false);
        let mut quote = Vec::from(&complete[..20]);
        quote.extend_from_slice(&[0; 20]);
        quote[0] = 0x4a; // 40-byte IPv4 header; no transport bytes fit.
        quote[2..4].copy_from_slice(&48u16.to_be_bytes());
        quote[10..12].fill(0);
        let inner_checksum = !checksum(&quote, 0);
        quote[10..12].copy_from_slice(&inner_checksum.to_be_bytes());
        let mut error = icmp_error(&quote, [192, 0, 2, 1], [10, 0, 0, 2]);
        assert_eq!(error.len(), 68);
        assert_eq!(
            rewrite_ipv4_related_icmp(
                &mut error,
                from,
                to,
                NatManipSide::Source,
                CtChecksumMode::Skip,
            ),
            Err(NatRewriteError::InvalidPacket)
        );
        rewrite_ipv4_generated_related_icmp(
            &mut error,
            from,
            to,
            NatManipSide::Source,
            CtChecksumMode::Skip,
        )
        .unwrap();
        assert_eq!(&error[12..16], &[203, 0, 113, 1]);
        assert_eq!(&error[44..48], &[203, 0, 113, 1]);
        assert_eq!(checksum(&error[..20], 0), u16::MAX);
        assert_eq!(checksum(&error[20..], 0), u16::MAX);
        assert_eq!(checksum(&error[28..68], 0), u16::MAX);
    }

    #[test]
    fn related_reply_direction_translates_to_original_reverse() {
        let original = tuple([10, 0, 0, 2], [198, 51, 100, 8], 1234, 80);
        let wire = tuple([203, 0, 113, 9], [192, 0, 2, 3], 40000, 8080);
        let from = wire.reverse().unwrap();
        let target = original.reverse().unwrap();
        let after_destination = tuple([198, 51, 100, 8], [203, 0, 113, 9], 80, 40000);
        let mut packet = icmp_error(&udp_packet(from, true), [192, 0, 2, 99], [192, 0, 2, 3]);
        rewrite_ipv4_related_icmp(
            &mut packet,
            from,
            after_destination,
            NatManipSide::Destination,
            CtChecksumMode::Verify,
        )
        .unwrap();
        rewrite_ipv4_related_icmp(
            &mut packet,
            after_destination,
            target,
            NatManipSide::Source,
            CtChecksumMode::Verify,
        )
        .unwrap();
        assert_related_checksums(&packet, target);
        assert_eq!(&packet[12..20], &[10, 0, 0, 2, 198, 51, 100, 8]);
        assert_eq!(&packet[28..], udp_packet(target, true));
    }

    #[test]
    fn related_tcp_minimum_quote_does_not_read_absent_checksum() {
        let from = CtTuple {
            src: [203, 0, 113, 9].into(),
            dst: [192, 0, 2, 3].into(),
            l4: CtL4::Tcp {
                src_port: 40000,
                dst_port: 443,
            },
        };
        let to = CtTuple {
            src: [10, 0, 0, 2].into(),
            dst: from.dst,
            l4: CtL4::Tcp {
                src_port: 1234,
                dst_port: 443,
            },
        };
        let mut packet = icmp_error(&tcp_packet(from)[..28], [192, 0, 2, 3], [203, 0, 113, 9]);
        rewrite_ipv4_related_icmp(
            &mut packet,
            from,
            to,
            NatManipSide::Destination,
            CtChecksumMode::Verify,
        )
        .unwrap();
        assert_eq!(packet.len(), 56);
        assert_related_checksums(&packet, to);
        assert_eq!(&packet[48..50], &1234u16.to_be_bytes());

        // When the quote does carry the whole header, the original TCP
        // checksum remains valid after the address and port translation.
        let mut full_quote = icmp_error(&tcp_packet(from), [192, 0, 2, 3], [203, 0, 113, 9]);
        rewrite_ipv4_related_icmp(
            &mut full_quote,
            from,
            to,
            NatManipSide::Destination,
            CtChecksumMode::Verify,
        )
        .unwrap();
        assert_related_checksums(&full_quote, to);
        assert_eq!(&full_quote[28..], tcp_packet(to));
    }

    #[test]
    fn related_icmp_identifier_can_change_at_source_hook() {
        let from = CtTuple {
            src: [10, 0, 0, 2].into(),
            dst: [192, 0, 2, 3].into(),
            l4: CtL4::Icmp {
                identifier: 1234,
                kind: 8,
                code: 0,
            },
        };
        let to = CtTuple {
            src: from.src,
            dst: [198, 51, 100, 8].into(),
            l4: CtL4::Icmp {
                identifier: 40000,
                kind: 8,
                code: 0,
            },
        };
        let mut packet = icmp_error(&icmp_packet(from), [192, 0, 2, 3], [10, 0, 0, 2]);
        rewrite_ipv4_related_icmp(
            &mut packet,
            from,
            to,
            NatManipSide::Source,
            CtChecksumMode::Verify,
        )
        .unwrap();
        assert_related_checksums(&packet, to);
        assert_eq!(checksum(&packet[48..], 0), u16::MAX);
        assert_eq!(&packet[12..16], &[198, 51, 100, 8]);
    }

    #[test]
    fn related_rejection_never_partially_rewrites() {
        let from = tuple([203, 0, 113, 9], [192, 0, 2, 3], 40000, 8080);
        let to = tuple([10, 0, 0, 2], [192, 0, 2, 3], 1234, 8080);
        let packet = icmp_error(&udp_packet(from, true), [192, 0, 2, 3], [203, 0, 113, 9]);
        let mut wrong_outer = packet.clone();
        wrong_outer[16..20].copy_from_slice(&[10, 0, 0, 99]);
        wrong_outer[10..12].fill(0);
        let ip_checksum = !checksum(&wrong_outer[..20], 0);
        wrong_outer[10..12].copy_from_slice(&ip_checksum.to_be_bytes());
        let mut bad_checksum = packet.clone();
        bad_checksum[22] ^= 1;
        let mut short_quote = icmp_error(
            &udp_packet(from, true)[..27],
            [192, 0, 2, 3],
            [203, 0, 113, 9],
        );
        for (input, expected) in [
            (packet.clone(), NatRewriteError::Unsupported),
            (wrong_outer, NatRewriteError::TupleMismatch),
            (bad_checksum, NatRewriteError::InvalidPacket),
            (short_quote.clone(), NatRewriteError::InvalidPacket),
        ] {
            let mut copy = input.clone();
            let target = if expected == NatRewriteError::Unsupported {
                tuple([10, 0, 0, 2], [192, 0, 2, 4], 1234, 8080)
            } else {
                to
            };
            assert_eq!(
                rewrite_ipv4_related_icmp(
                    &mut copy,
                    from,
                    target,
                    NatManipSide::Destination,
                    CtChecksumMode::Verify,
                ),
                Err(expected)
            );
            assert_eq!(copy, input);
        }
        let mut mismatched_quote = packet.clone();
        let wrong_from = tuple([203, 0, 113, 10], [192, 0, 2, 3], 40000, 8080);
        assert_eq!(
            rewrite_ipv4_related_icmp(
                &mut mismatched_quote,
                wrong_from,
                to,
                NatManipSide::Destination,
                CtChecksumMode::Verify,
            ),
            Err(NatRewriteError::TupleMismatch)
        );
        assert_eq!(mismatched_quote, packet);
        let mut mixed_family = packet.clone();
        let v6_target = CtTuple {
            src: CtAddress::V6([0; 16]),
            ..to
        };
        assert_eq!(
            rewrite_ipv4_related_icmp(
                &mut mixed_family,
                from,
                v6_target,
                NatManipSide::Destination,
                CtChecksumMode::Verify,
            ),
            Err(NatRewriteError::Unsupported)
        );
        assert_eq!(mixed_family, packet);
        // A LOCAL_OUT checksum-offload packet is structurally valid even if
        // its outer ICMP checksum is not yet complete.
        assert!(rewrite_ipv4_related_icmp(
            &mut short_quote,
            from,
            to,
            NatManipSide::Destination,
            CtChecksumMode::Skip,
        )
        .is_err());
        let mut offloaded = packet.clone();
        offloaded[22] ^= 1;
        rewrite_ipv4_related_icmp(
            &mut offloaded,
            from,
            to,
            NatManipSide::Destination,
            CtChecksumMode::Skip,
        )
        .unwrap();
        assert_related_checksums(&offloaded, to);
    }

    #[test]
    fn generic_ipv4_rewrites_only_ip_addresses() {
        let from = CtTuple {
            src: [10, 0, 0, 2].into(),
            dst: [203, 0, 113, 1].into(),
            l4: CtL4::Generic { protocol: 47 },
        };
        let to = CtTuple {
            src: [192, 0, 2, 1].into(),
            dst: [10, 0, 0, 3].into(),
            ..from
        };
        let mut packet = Vec::from([0u8; 24]);
        packet[0] = 0x45;
        packet[2..4].copy_from_slice(&24u16.to_be_bytes());
        packet[8] = 64;
        packet[9] = 47;
        packet[12..16].copy_from_slice(&[10, 0, 0, 2]);
        packet[16..20].copy_from_slice(&[203, 0, 113, 1]);
        packet[20..24].copy_from_slice(b"GRE!");
        let check = !checksum(&packet[..20], 0);
        packet[10..12].copy_from_slice(&check.to_be_bytes());
        rewrite_ipv4_tuple(&mut packet, from, to, CtChecksumMode::Verify).unwrap();
        assert_eq!(&packet[20..], b"GRE!");
        assert_eq!(
            parse_ipv4_conntrack(&packet),
            ParsedCtPacket::Flow {
                tuple: to,
                kind: super::super::CtPacketKind::Generic,
            }
        );
    }
}
