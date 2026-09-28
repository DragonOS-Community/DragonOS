//! Strict IPv4/L4 parsing at the conntrack boundary.
//!
//! Only complete datagrams enter this module. Unknown protocols remain
//! untracked; malformed supported protocols are INVALID, never a partial key.

use super::{
    tcp::{TcpOptions, TcpSegment},
    CtAddress, CtL4, CtPacketKind, CtTuple,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ParsedCtPacket {
    Flow {
        tuple: CtTuple,
        kind: CtPacketKind,
    },
    Related {
        quoted: CtTuple,
        outer_destination: CtAddress,
    },
    Untracked,
    Invalid,
}

/// The receive-side PRE_ROUTING path may verify transport checksums before
/// creating a flow. Linux conntrack does not perform that verification in
/// LOCAL_OUT: raw sockets and checksum-offloaded output can reach that hook
/// with a partial or intentionally supplied transport checksum. Structural
/// length and protocol checks are required in both modes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CtChecksumMode {
    Verify,
    Skip,
}

/// `packet` must contain a complete IPv4 datagram, not an L2 frame. Unlike a
/// transport socket parser, this also accepts ICMP error quotes of only eight
/// transport bytes, which is the minimum Linux ICMP payload requirement.
pub(crate) fn parse_ipv4_conntrack(packet: &[u8]) -> ParsedCtPacket {
    parse_ipv4_conntrack_with_mode(packet, CtChecksumMode::Verify)
}

pub(crate) fn parse_ipv4_conntrack_with_mode(
    packet: &[u8],
    checksum_mode: CtChecksumMode,
) -> ParsedCtPacket {
    let Some(Ipv4Header {
        header_len,
        total_len,
        source,
        destination,
        protocol,
    }) = ipv4_header(packet, true)
    else {
        return ParsedCtPacket::Invalid;
    };
    let payload = &packet[header_len..total_len];
    let l4 = match protocol {
        6 => parse_tcp(payload, source, destination, checksum_mode),
        17 => parse_udp(payload, source, destination, checksum_mode),
        1 => return parse_icmp(payload, source, destination, checksum_mode),
        protocol => {
            return ParsedCtPacket::Flow {
                tuple: CtTuple {
                    src: source.into(),
                    dst: destination.into(),
                    l4: CtL4::Generic { protocol },
                },
                kind: CtPacketKind::Generic,
            };
        }
    };
    match l4 {
        Some((l4, kind)) => ParsedCtPacket::Flow {
            tuple: CtTuple {
                src: source.into(),
                dst: destination.into(),
                l4,
            },
            kind,
        },
        None => ParsedCtPacket::Invalid,
    }
}

pub(super) struct Ipv4Header {
    pub(super) header_len: usize,
    pub(super) total_len: usize,
    pub(super) source: [u8; 4],
    pub(super) destination: [u8; 4],
    pub(super) protocol: u8,
}

pub(super) fn ipv4_header(packet: &[u8], complete: bool) -> Option<Ipv4Header> {
    if packet.len() < 20 || packet[0] >> 4 != 4 {
        return None;
    }
    let header_len = usize::from(packet[0] & 0x0f) * 4;
    if !(20..=60).contains(&header_len) || packet.len() < header_len {
        return None;
    }
    let total_len = usize::from(u16::from_be_bytes([packet[2], packet[3]]));
    if total_len < header_len || (complete && total_len > packet.len()) {
        return None;
    }
    let flags_offset = u16::from_be_bytes([packet[6], packet[7]]);
    if flags_offset & 0x3fff != 0 || checksum(&packet[..header_len], 0) != u16::MAX {
        return None;
    }
    Some(Ipv4Header {
        header_len,
        total_len,
        source: packet[12..16].try_into().ok()?,
        destination: packet[16..20].try_into().ok()?,
        protocol: packet[9],
    })
}

fn parse_tcp(
    payload: &[u8],
    src: [u8; 4],
    dst: [u8; 4],
    checksum_mode: CtChecksumMode,
) -> Option<(CtL4, CtPacketKind)> {
    if payload.len() < 20
        || (checksum_mode == CtChecksumMode::Verify && !transport_checksum(payload, src, dst, 6))
    {
        return None;
    }
    let header_len = usize::from(payload[12] >> 4) * 4;
    if !(20..=60).contains(&header_len) || header_len > payload.len() {
        return None;
    }
    let flags = payload[13];
    let syn = flags & 0x02 != 0;
    let segment = TcpSegment {
        seq: u32::from_be_bytes(payload[4..8].try_into().ok()?),
        ack_seq: u32::from_be_bytes(payload[8..12].try_into().ok()?),
        window: u16::from_be_bytes([payload[14], payload[15]]),
        payload_len: u16::try_from(payload.len() - header_len).ok()?,
        syn,
        ack: flags & 0x10 != 0,
        fin: flags & 0x01 != 0,
        rst: flags & 0x04 != 0,
        urg: flags & 0x20 != 0,
        options: tcp_options(&payload[20..header_len], syn),
    };
    if !segment.valid() {
        return None;
    }
    Some((
        CtL4::Tcp {
            src_port: u16::from_be_bytes([payload[0], payload[1]]),
            dst_port: u16::from_be_bytes([payload[2], payload[3]]),
        },
        CtPacketKind::Tcp(segment),
    ))
}

pub(super) fn tcp_options(options: &[u8], syn: bool) -> TcpOptions {
    let mut parsed = TcpOptions::default();
    let mut index = 0;
    while index < options.len() {
        let kind = options[index];
        if kind == 0 {
            break;
        }
        if kind == 1 {
            index += 1;
            continue;
        }
        let Some(&length) = options.get(index + 1) else {
            break;
        };
        let length = usize::from(length);
        if length < 2 || index + length > options.len() {
            break;
        }
        let value = &options[index + 2..index + length];
        match (kind, value.len()) {
            (3, 1) if syn => parsed.window_scale = Some(value[0].min(14)),
            (4, 0) if syn => parsed.sack_permitted = true,
            (5, bytes) if bytes >= 8 && bytes % 8 == 0 => {
                for block in value.chunks_exact(8) {
                    let right = u32::from_be_bytes(block[4..8].try_into().unwrap());
                    if parsed
                        .highest_sack
                        .is_none_or(|current| (right.wrapping_sub(current) as i32) > 0)
                    {
                        parsed.highest_sack = Some(right);
                    }
                }
            }
            _ => {}
        }
        index += length;
    }
    parsed
}

fn parse_udp(
    payload: &[u8],
    src: [u8; 4],
    dst: [u8; 4],
    checksum_mode: CtChecksumMode,
) -> Option<(CtL4, CtPacketKind)> {
    if payload.len() < 8 {
        return None;
    }
    let length = usize::from(u16::from_be_bytes([payload[4], payload[5]]));
    if length < 8 || length > payload.len() {
        return None;
    }
    // IPv4 permits a zero UDP checksum. If present, checksum only the UDP
    // length, not any IP padding after it.
    if checksum_mode == CtChecksumMode::Verify
        && (payload[6] != 0 || payload[7] != 0)
        && !transport_checksum(&payload[..length], src, dst, 17)
    {
        return None;
    }
    Some((
        CtL4::Udp {
            src_port: u16::from_be_bytes([payload[0], payload[1]]),
            dst_port: u16::from_be_bytes([payload[2], payload[3]]),
        },
        CtPacketKind::Udp,
    ))
}

fn parse_icmp(
    payload: &[u8],
    source: [u8; 4],
    destination: [u8; 4],
    checksum_mode: CtChecksumMode,
) -> ParsedCtPacket {
    if payload.len() < 8
        || (checksum_mode == CtChecksumMode::Verify && checksum(payload, 0) != u16::MAX)
    {
        return ParsedCtPacket::Invalid;
    }
    let kind = payload[0];
    let code = payload[1];
    // Linux 6.6 nf_conntrack_proto_icmp.c rejects types above 18 rather than
    // classifying them as an otherwise valid untracked protocol.
    if kind > 18 {
        return ParsedCtPacket::Invalid;
    }
    if matches!(kind, 0 | 8 | 13..=18) {
        return ParsedCtPacket::Flow {
            tuple: CtTuple {
                src: source.into(),
                dst: destination.into(),
                l4: CtL4::Icmp {
                    identifier: u16::from_be_bytes([payload[4], payload[5]]),
                    kind,
                    code,
                },
            },
            kind: CtPacketKind::IcmpQuery,
        };
    }
    if !matches!(kind, 3 | 4 | 5 | 11 | 12) {
        return ParsedCtPacket::Untracked;
    }
    let quote = &payload[8..];
    let Some((header_len, quoted_src, quoted_dst, protocol)) = ipv4_quote_header(quote) else {
        return ParsedCtPacket::Invalid;
    };
    let l4 = &quote[header_len..];
    let quoted_l4 = match protocol {
        6 if l4.len() >= 8 => CtL4::Tcp {
            src_port: u16::from_be_bytes([l4[0], l4[1]]),
            dst_port: u16::from_be_bytes([l4[2], l4[3]]),
        },
        17 if l4.len() >= 8 => CtL4::Udp {
            src_port: u16::from_be_bytes([l4[0], l4[1]]),
            dst_port: u16::from_be_bytes([l4[2], l4[3]]),
        },
        1 if l4.len() >= 8 && matches!(l4[0], 0 | 8 | 13..=18) => CtL4::Icmp {
            identifier: u16::from_be_bytes([l4[4], l4[5]]),
            kind: l4[0],
            code: l4[1],
        },
        protocol if !matches!(protocol, 1 | 6 | 17) => CtL4::Generic { protocol },
        _ => return ParsedCtPacket::Invalid,
    };
    ParsedCtPacket::Related {
        quoted: CtTuple {
            src: quoted_src.into(),
            dst: quoted_dst.into(),
            l4: quoted_l4,
        },
        outer_destination: destination.into(),
    }
}

/// Linux's ICMP-error conntrack path reads a *quote*, not a newly received IP
/// packet: the embedded total length may exceed the available quote and its
/// old header checksum is not revalidated. Still reject non-first fragments
/// and out-of-bounds IHL before reading the eight quoted L4 bytes.
fn ipv4_quote_header(quote: &[u8]) -> Option<(usize, [u8; 4], [u8; 4], u8)> {
    if quote.len() < 20 || quote[0] >> 4 != 4 {
        return None;
    }
    let header_len = usize::from(quote[0] & 0x0f) * 4;
    if !(20..=60).contains(&header_len) || header_len > quote.len() {
        return None;
    }
    let fragment_offset = u16::from_be_bytes([quote[6], quote[7]]) & 0x1fff;
    if fragment_offset != 0 {
        return None;
    }
    Some((
        header_len,
        quote[12..16].try_into().ok()?,
        quote[16..20].try_into().ok()?,
        quote[9],
    ))
}

pub(super) fn checksum(bytes: &[u8], initial: u32) -> u16 {
    let mut sum = initial;
    for word in bytes.chunks_exact(2) {
        sum += u32::from(u16::from_be_bytes([word[0], word[1]]));
    }
    if bytes.len() & 1 != 0 {
        sum += u32::from(bytes[bytes.len() - 1]) << 8;
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    sum as u16
}

fn transport_checksum(payload: &[u8], src: [u8; 4], dst: [u8; 4], protocol: u8) -> bool {
    let initial = u32::from(u16::from_be_bytes([src[0], src[1]]))
        + u32::from(u16::from_be_bytes([src[2], src[3]]))
        + u32::from(u16::from_be_bytes([dst[0], dst[1]]))
        + u32::from(u16::from_be_bytes([dst[2], dst[3]]))
        + u32::from(protocol)
        + payload.len() as u32;
    checksum(payload, initial) == u16::MAX
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    const SRC: [u8; 4] = [10, 0, 0, 1];
    const DST: [u8; 4] = [10, 0, 0, 2];

    fn ip(protocol: u8, payload: &[u8]) -> Vec<u8> {
        let mut packet = Vec::from([0u8; 20]);
        packet[0] = 0x45;
        packet[2..4].copy_from_slice(&u16::try_from(20 + payload.len()).unwrap().to_be_bytes());
        packet[8] = 64;
        packet[9] = protocol;
        packet[12..16].copy_from_slice(&SRC);
        packet[16..20].copy_from_slice(&DST);
        let sum = !checksum(&packet, 0);
        packet[10..12].copy_from_slice(&sum.to_be_bytes());
        packet.extend_from_slice(payload);
        packet
    }

    fn udp(checksum_enabled: bool) -> Vec<u8> {
        let mut l4 = Vec::from([0u8; 12]);
        l4[0..2].copy_from_slice(&1234u16.to_be_bytes());
        l4[2..4].copy_from_slice(&8080u16.to_be_bytes());
        l4[4..6].copy_from_slice(&12u16.to_be_bytes());
        l4[8..12].copy_from_slice(b"data");
        if checksum_enabled {
            let initial = u32::from(u16::from_be_bytes([SRC[0], SRC[1]]))
                + u32::from(u16::from_be_bytes([SRC[2], SRC[3]]))
                + u32::from(u16::from_be_bytes([DST[0], DST[1]]))
                + u32::from(u16::from_be_bytes([DST[2], DST[3]]))
                + 17
                + l4.len() as u32;
            let sum = !checksum(&l4, initial);
            l4[6..8].copy_from_slice(&sum.to_be_bytes());
        }
        ip(17, &l4)
    }

    #[test]
    fn udp_zero_checksum_is_valid_but_nonzero_corruption_is_invalid() {
        assert!(matches!(
            parse_ipv4_conntrack(&udp(false)),
            ParsedCtPacket::Flow {
                kind: CtPacketKind::Udp,
                ..
            }
        ));
        let mut packet = udp(true);
        assert!(matches!(
            parse_ipv4_conntrack(&packet),
            ParsedCtPacket::Flow {
                kind: CtPacketKind::Udp,
                ..
            }
        ));
        packet[28] ^= 1;
        assert_eq!(parse_ipv4_conntrack(&packet), ParsedCtPacket::Invalid);
        assert!(matches!(
            parse_ipv4_conntrack_with_mode(&packet, CtChecksumMode::Skip),
            ParsedCtPacket::Flow {
                kind: CtPacketKind::Udp,
                ..
            }
        ));
    }

    #[test]
    fn fragments_and_bad_ip_checksum_never_create_a_tuple() {
        let mut packet = udp(false);
        packet[6] = 0x20;
        packet[10..12].fill(0);
        let sum = !checksum(&packet[..20], 0);
        packet[10..12].copy_from_slice(&sum.to_be_bytes());
        assert_eq!(parse_ipv4_conntrack(&packet), ParsedCtPacket::Invalid);
        packet[6] = 0;
        packet[10..12].fill(0);
        let sum = !checksum(&packet[..20], 0);
        packet[10..12].copy_from_slice(&sum.to_be_bytes());
        packet[10] ^= 1;
        assert_eq!(parse_ipv4_conntrack(&packet), ParsedCtPacket::Invalid);
        assert_eq!(
            parse_ipv4_conntrack_with_mode(&packet, CtChecksumMode::Skip),
            ParsedCtPacket::Invalid
        );
    }

    #[test]
    fn tcp_syn_options_and_full_checksum_are_parsed() {
        let mut l4 = Vec::from([0u8; 28]);
        l4[0..2].copy_from_slice(&1234u16.to_be_bytes());
        l4[2..4].copy_from_slice(&80u16.to_be_bytes());
        l4[4..8].copy_from_slice(&12u32.to_be_bytes());
        l4[12] = 7 << 4;
        l4[13] = 0x02;
        l4[14..16].copy_from_slice(&4096u16.to_be_bytes());
        l4[20..28].copy_from_slice(&[3, 3, 7, 4, 2, 1, 1, 0]);
        let initial = u32::from(u16::from_be_bytes([SRC[0], SRC[1]]))
            + u32::from(u16::from_be_bytes([SRC[2], SRC[3]]))
            + u32::from(u16::from_be_bytes([DST[0], DST[1]]))
            + u32::from(u16::from_be_bytes([DST[2], DST[3]]))
            + 6
            + l4.len() as u32;
        let sum = !checksum(&l4, initial);
        l4[16..18].copy_from_slice(&sum.to_be_bytes());
        let mut packet = ip(6, &l4);
        let ParsedCtPacket::Flow {
            kind: CtPacketKind::Tcp(segment),
            ..
        } = parse_ipv4_conntrack(&packet)
        else {
            panic!("valid SYN must parse")
        };
        assert_eq!(segment.seq, 12);
        assert_eq!(segment.options.window_scale, Some(7));
        assert!(segment.options.sack_permitted);
        packet[20 + 20 + 2] ^= 1;
        assert_eq!(parse_ipv4_conntrack(&packet), ParsedCtPacket::Invalid);
        assert!(matches!(
            parse_ipv4_conntrack_with_mode(&packet, CtChecksumMode::Skip),
            ParsedCtPacket::Flow {
                kind: CtPacketKind::Tcp(_),
                ..
            }
        ));
    }

    #[test]
    fn icmp_error_accepts_minimal_quoted_udp_header() {
        for damage_quote_checksum in [false, true] {
            let mut quoted_packet = udp(false);
            if damage_quote_checksum {
                // Linux does not revalidate an embedded IPv4 checksum: the
                // outer ICMP checksum still covers these exact quote bytes.
                quoted_packet[10] ^= 1;
            }
            let mut icmp = Vec::from([0u8; 8]);
            icmp[0] = 3;
            icmp[1] = 3;
            icmp.extend_from_slice(&quoted_packet[..28]);
            let sum = !checksum(&icmp, 0);
            icmp[2..4].copy_from_slice(&sum.to_be_bytes());
            let mut packet = ip(1, &icmp);
            // Real ICMP error travels back to the original sender.
            packet[12..16].copy_from_slice(&DST);
            packet[16..20].copy_from_slice(&SRC);
            packet[10..12].fill(0);
            let sum = !checksum(&packet[..20], 0);
            packet[10..12].copy_from_slice(&sum.to_be_bytes());
            let ParsedCtPacket::Related {
                quoted,
                outer_destination,
            } = parse_ipv4_conntrack(&packet)
            else {
                panic!("minimal ICMP quote must parse")
            };
            assert_eq!(quoted.src, CtAddress::V4(SRC));
            assert_eq!(quoted.dst, CtAddress::V4(DST));
            assert_eq!(outer_destination, CtAddress::V4(SRC));
            assert_eq!(
                quoted.l4,
                CtL4::Udp {
                    src_port: 1234,
                    dst_port: 8080
                }
            );
        }
    }

    #[test]
    fn unknown_high_icmp_type_is_invalid_not_untracked() {
        let mut icmp = [0u8; 8];
        icmp[0] = 255;
        let sum = !checksum(&icmp, 0);
        icmp[2..4].copy_from_slice(&sum.to_be_bytes());
        assert_eq!(parse_ipv4_conntrack(&ip(1, &icmp)), ParsedCtPacket::Invalid);
    }

    #[test]
    fn unknown_transport_uses_protocol_scoped_generic_tuple() {
        let packet = ip(47, &[1, 2, 3, 4]);
        assert_eq!(
            parse_ipv4_conntrack(&packet),
            ParsedCtPacket::Flow {
                tuple: CtTuple {
                    src: SRC.into(),
                    dst: DST.into(),
                    l4: CtL4::Generic { protocol: 47 },
                },
                kind: CtPacketKind::Generic,
            }
        );
        let other = ip(50, &[1, 2, 3, 4]);
        assert_ne!(parse_ipv4_conntrack(&packet), parse_ipv4_conntrack(&other));
    }

    #[test]
    fn output_checksum_skip_keeps_icmp_structure_validation() {
        let mut icmp = [0u8; 8];
        icmp[0] = 8;
        icmp[4..6].copy_from_slice(&1234u16.to_be_bytes());
        let mut packet = ip(1, &icmp);
        assert_eq!(parse_ipv4_conntrack(&packet), ParsedCtPacket::Invalid);
        assert!(matches!(
            parse_ipv4_conntrack_with_mode(&packet, CtChecksumMode::Skip),
            ParsedCtPacket::Flow {
                kind: CtPacketKind::IcmpQuery,
                ..
            }
        ));
        packet.truncate(27);
        assert_eq!(
            parse_ipv4_conntrack_with_mode(&packet, CtChecksumMode::Skip),
            ParsedCtPacket::Invalid
        );
    }
}
