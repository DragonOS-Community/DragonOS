//! IPv6 tuple extraction for the conntrack boundary.
//!
//! The caller supplies a complete IP datagram after receive-side defragmentation.
//! A quoted datagram inside an ICMPv6 error is different: its payload is allowed
//! to end after the first eight transport bytes. This module does not route,
//! rewrite, or confirm a flow.

use super::{
    packet::{checksum, tcp_options, CtChecksumMode, ParsedCtPacket},
    tcp::TcpSegment,
    CtL4, CtPacketKind, CtTuple,
};

pub(super) struct Ipv6Header<'a> {
    pub(super) bytes: &'a [u8],
    pub(super) source: [u8; 16],
    pub(super) destination: [u8; 16],
    pub(super) protocol: u8,
    pub(super) transport_offset: usize,
    hop_limit: u8,
}

/// PRE_ROUTING uses `Verify`; LOCAL_OUT can use `Skip`, as in Linux 6.6.
pub(crate) fn parse_ipv6_conntrack(packet: &[u8]) -> ParsedCtPacket {
    parse_ipv6_conntrack_with_mode(packet, CtChecksumMode::Verify)
}

pub(crate) fn parse_ipv6_conntrack_with_mode(
    packet: &[u8],
    checksum_mode: CtChecksumMode,
) -> ParsedCtPacket {
    let Some(header) = ipv6_header(packet, false) else {
        return ParsedCtPacket::Invalid;
    };
    let payload = &header.bytes[header.transport_offset..];
    let parsed = match header.protocol {
        6 => parse_tcp(payload, header.source, header.destination, checksum_mode),
        17 => parse_udp(payload, header.source, header.destination, checksum_mode),
        58 => return parse_icmpv6(payload, &header, checksum_mode),
        protocol => {
            return ParsedCtPacket::Flow {
                tuple: CtTuple {
                    src: header.source.into(),
                    dst: header.destination.into(),
                    l4: CtL4::Generic { protocol },
                },
                kind: CtPacketKind::Generic,
            };
        }
    };
    match parsed {
        Some((l4, kind)) => ParsedCtPacket::Flow {
            tuple: CtTuple {
                src: header.source.into(),
                dst: header.destination.into(),
                l4,
            },
            kind,
        },
        None => ParsedCtPacket::Invalid,
    }
}

/// Walk the same reachable extension-header types as Linux 6.6
/// `ipv6_skip_exthdr`: Hop-by-Hop, Routing, Fragment, AH and Destination.
/// ESP stays opaque. Non-atomic outer fragments require defragmentation before
/// conntrack; only an offset-zero fragment may appear inside an error quote.
pub(super) fn ipv6_header(packet: &[u8], quoted: bool) -> Option<Ipv6Header<'_>> {
    if packet.len() < 40 || packet[0] >> 4 != 6 {
        return None;
    }
    let payload_len = usize::from(u16::from_be_bytes([packet[4], packet[5]]));
    let first_next = packet[6];
    // A zero Payload Length with Hop-by-Hop may denote a Jumbo Payload.
    // This bounded parser does not support its 32-bit length option.
    if payload_len == 0 && first_next == 0 {
        return None;
    }
    let declared_end = 40 + payload_len;
    if !quoted && declared_end > packet.len() {
        return None;
    }
    let bytes = &packet[..declared_end.min(packet.len())];
    let source = bytes[8..24].try_into().ok()?;
    let destination = bytes[24..40].try_into().ok()?;
    let mut next = first_next;
    let mut offset = 40usize;
    loop {
        let length = match next {
            0 | 43 | 60 => {
                let header = bytes.get(offset..offset + 2)?;
                let length = (usize::from(header[1]) + 1) * 8;
                let following = header[0];
                next = following;
                length
            }
            44 => {
                let header = bytes.get(offset..offset + 8)?;
                let flags = u16::from_be_bytes([header[2], header[3]]);
                if flags & 0x0006 != 0 || flags & 0xfff8 != 0 || (!quoted && flags & 1 != 0) {
                    return None;
                }
                next = header[0];
                8
            }
            51 => {
                let header = bytes.get(offset..offset + 2)?;
                let length = (usize::from(header[1]) + 2) * 4;
                next = header[0];
                length
            }
            59 => return None,
            _ => break,
        };
        offset = offset.checked_add(length)?;
        if offset > bytes.len() {
            return None;
        }
    }
    Some(Ipv6Header {
        bytes,
        source,
        destination,
        protocol: next,
        transport_offset: offset,
        hop_limit: packet[7],
    })
}

fn parse_tcp(
    payload: &[u8],
    source: [u8; 16],
    destination: [u8; 16],
    checksum_mode: CtChecksumMode,
) -> Option<(CtL4, CtPacketKind)> {
    if payload.len() < 20
        || (checksum_mode == CtChecksumMode::Verify
            && !transport_checksum(payload, source, destination, 6))
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

fn parse_udp(
    payload: &[u8],
    source: [u8; 16],
    destination: [u8; 16],
    checksum_mode: CtChecksumMode,
) -> Option<(CtL4, CtPacketKind)> {
    if payload.len() < 8 {
        return None;
    }
    let length = usize::from(u16::from_be_bytes([payload[4], payload[5]]));
    if length < 8 || length > payload.len() {
        return None;
    }
    // Linux 6.6's conntrack udp_error() accepts a zero checksum; IPv6 UDP
    // socket delivery may reject it later. A nonzero checksum is checked only
    // at PRE_ROUTING, using the declared UDP length rather than trailing bytes.
    if checksum_mode == CtChecksumMode::Verify
        && (payload[6] != 0 || payload[7] != 0)
        && !transport_checksum(&payload[..length], source, destination, 17)
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

fn parse_icmpv6(
    payload: &[u8],
    header: &Ipv6Header<'_>,
    checksum_mode: CtChecksumMode,
) -> ParsedCtPacket {
    if payload.len() < 8
        || (checksum_mode == CtChecksumMode::Verify
            && !transport_checksum(payload, header.source, header.destination, 58))
    {
        return ParsedCtPacket::Invalid;
    }
    let kind = payload[0];
    let code = payload[1];
    // Linux excludes MLD and Neighbor Discovery control traffic from CT.
    if matches!(kind, 130..=136 | 143) {
        return ParsedCtPacket::Untracked;
    }
    if matches!(kind, 128 | 129 | 139 | 140) {
        // For NI query/reply the Linux icmp6_identifier union aliases Qtype
        // (bytes 4..6), not the eight-byte nonce following the ICMP header.
        return ParsedCtPacket::Flow {
            tuple: CtTuple {
                src: header.source.into(),
                dst: header.destination.into(),
                l4: CtL4::Icmpv6 {
                    identifier: u16::from_be_bytes([payload[4], payload[5]]),
                    kind,
                    code,
                },
            },
            kind: CtPacketKind::IcmpQuery,
        };
    }
    let quote = if kind == 137 {
        // Linux treats a valid Redirect Header option as an ICMP error quote.
        // Other Redirect options do not attach a RELATED conntrack entry.
        if code != 0 {
            return ParsedCtPacket::Untracked;
        }
        if header.hop_limit != 255 || !is_link_local(header.source) || payload.len() < 42 {
            return ParsedCtPacket::Invalid;
        }
        if payload[41] == 0 {
            return ParsedCtPacket::Invalid;
        }
        if payload[40] != 4 {
            return ParsedCtPacket::Untracked;
        }
        if payload.len() < 48 {
            return ParsedCtPacket::Invalid;
        }
        &payload[48..]
    } else if kind < 128 {
        &payload[8..]
    } else {
        return ParsedCtPacket::Untracked;
    };
    let Some(quoted) = ipv6_header(quote, true) else {
        return ParsedCtPacket::Invalid;
    };
    let l4 = &quoted.bytes[quoted.transport_offset..];
    let quoted_l4 = match quoted.protocol {
        6 if l4.len() >= 8 => CtL4::Tcp {
            src_port: u16::from_be_bytes([l4[0], l4[1]]),
            dst_port: u16::from_be_bytes([l4[2], l4[3]]),
        },
        17 if l4.len() >= 8 => CtL4::Udp {
            src_port: u16::from_be_bytes([l4[0], l4[1]]),
            dst_port: u16::from_be_bytes([l4[2], l4[3]]),
        },
        58 if l4.len() >= 8 && matches!(l4[0], 128 | 129 | 139 | 140) => CtL4::Icmpv6 {
            identifier: u16::from_be_bytes([l4[4], l4[5]]),
            kind: l4[0],
            code: l4[1],
        },
        protocol if !matches!(protocol, 6 | 17 | 58) => CtL4::Generic { protocol },
        _ => return ParsedCtPacket::Invalid,
    };
    ParsedCtPacket::Related {
        quoted: CtTuple {
            src: quoted.source.into(),
            dst: quoted.destination.into(),
            l4: quoted_l4,
        },
        outer_destination: header.destination.into(),
    }
}

fn is_link_local(address: [u8; 16]) -> bool {
    address[0] == 0xfe && address[1] & 0xc0 == 0x80
}

fn transport_checksum(
    payload: &[u8],
    source: [u8; 16],
    destination: [u8; 16],
    protocol: u8,
) -> bool {
    let mut initial = 0u32;
    for word in source.chunks_exact(2).chain(destination.chunks_exact(2)) {
        initial += u32::from(u16::from_be_bytes([word[0], word[1]]));
    }
    let length = payload.len() as u32;
    initial += (length >> 16) + (length & 0xffff) + u32::from(protocol);
    checksum(payload, initial) == u16::MAX
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    const SRC: [u8; 16] = [0xfd, 0x35, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1];
    const DST: [u8; 16] = [0xfd, 0x35, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2];

    fn ip(next: u8, source: [u8; 16], destination: [u8; 16], payload: &[u8]) -> Vec<u8> {
        let mut packet = Vec::from([0u8; 40]);
        packet[0] = 0x60;
        packet[4..6].copy_from_slice(&u16::try_from(payload.len()).unwrap().to_be_bytes());
        packet[6] = next;
        packet[7] = 64;
        packet[8..24].copy_from_slice(&source);
        packet[24..40].copy_from_slice(&destination);
        packet.extend_from_slice(payload);
        packet
    }

    fn fill_checksum(
        payload: &mut [u8],
        offset: usize,
        protocol: u8,
        src: [u8; 16],
        dst: [u8; 16],
    ) {
        let mut sum = 0u32;
        for word in src.chunks_exact(2).chain(dst.chunks_exact(2)) {
            sum += u32::from(u16::from_be_bytes([word[0], word[1]]));
        }
        sum += payload.len() as u32 + u32::from(protocol);
        let value = !checksum(payload, sum);
        payload[offset..offset + 2].copy_from_slice(&value.to_be_bytes());
    }

    fn udp() -> Vec<u8> {
        let mut payload = Vec::from([0u8; 12]);
        payload[0..2].copy_from_slice(&1234u16.to_be_bytes());
        payload[2..4].copy_from_slice(&8080u16.to_be_bytes());
        payload[4..6].copy_from_slice(&12u16.to_be_bytes());
        payload[8..12].copy_from_slice(b"data");
        fill_checksum(&mut payload, 6, 17, SRC, DST);
        payload
    }

    fn icmp(kind: u8, src: [u8; 16], dst: [u8; 16], body: &[u8]) -> Vec<u8> {
        let mut payload = Vec::from([0u8; 8]);
        payload[0] = kind;
        payload.extend_from_slice(body);
        fill_checksum(&mut payload, 2, 58, src, dst);
        payload
    }

    #[test]
    fn udp_checksum_policy_and_length_are_independent() {
        let mut packet = ip(17, SRC, DST, &udp());
        assert!(matches!(
            parse_ipv6_conntrack(&packet),
            ParsedCtPacket::Flow {
                kind: CtPacketKind::Udp,
                ..
            }
        ));
        packet[50] ^= 1;
        assert_eq!(parse_ipv6_conntrack(&packet), ParsedCtPacket::Invalid);
        assert!(matches!(
            parse_ipv6_conntrack_with_mode(&packet, CtChecksumMode::Skip),
            ParsedCtPacket::Flow {
                kind: CtPacketKind::Udp,
                ..
            }
        ));
        packet[44..46].copy_from_slice(&13u16.to_be_bytes());
        assert_eq!(
            parse_ipv6_conntrack_with_mode(&packet, CtChecksumMode::Skip),
            ParsedCtPacket::Invalid
        );
        packet[44..46].copy_from_slice(&12u16.to_be_bytes());
        packet[46..48].fill(0);
        // Linux 6.6 conntrack's udp_error() skips validation for checksum zero.
        assert!(matches!(
            parse_ipv6_conntrack(&packet),
            ParsedCtPacket::Flow {
                kind: CtPacketKind::Udp,
                ..
            }
        ));
    }

    #[test]
    fn odd_udp_payload_uses_ipv6_pseudoheader_and_padding() {
        let mut payload = udp();
        payload.push(0x5a);
        payload[4..6].copy_from_slice(&13u16.to_be_bytes());
        payload[6..8].fill(0);
        fill_checksum(&mut payload, 6, 17, SRC, DST);
        let mut packet = ip(17, SRC, DST, &payload);
        assert!(matches!(
            parse_ipv6_conntrack(&packet),
            ParsedCtPacket::Flow { .. }
        ));
        packet[24] ^= 1; // Alter pseudoheader destination without touching UDP bytes.
        assert_eq!(parse_ipv6_conntrack(&packet), ParsedCtPacket::Invalid);
    }

    #[test]
    fn tcp_syn_options_and_extension_chain() {
        let mut tcp = Vec::from([0u8; 28]);
        tcp[0..2].copy_from_slice(&1234u16.to_be_bytes());
        tcp[2..4].copy_from_slice(&80u16.to_be_bytes());
        tcp[4..8].copy_from_slice(&17u32.to_be_bytes());
        tcp[12] = 7 << 4;
        tcp[13] = 2;
        tcp[14..16].copy_from_slice(&4096u16.to_be_bytes());
        tcp[20..28].copy_from_slice(&[3, 3, 7, 4, 2, 1, 1, 0]);
        fill_checksum(&mut tcp, 16, 6, SRC, DST);
        let mut extensions = Vec::from([43, 0, 0, 0, 0, 0, 0, 0]);
        extensions.extend_from_slice(&[60, 0, 0, 0, 0, 0, 0, 0]);
        extensions.extend_from_slice(&[6, 0, 0, 0, 0, 0, 0, 0]);
        extensions.extend_from_slice(&tcp);
        let packet = ip(0, SRC, DST, &extensions);
        let ParsedCtPacket::Flow {
            tuple,
            kind: CtPacketKind::Tcp(segment),
        } = parse_ipv6_conntrack(&packet)
        else {
            panic!("TCP after reachable IPv6 extensions must parse")
        };
        assert_eq!(tuple.src, SRC.into());
        assert_eq!(segment.seq, 17);
        assert_eq!(segment.options.window_scale, Some(7));
        assert!(segment.options.sack_permitted);
        let mut broken = packet;
        broken[41] = 7; // The first extension now extends beyond the datagram.
        assert_eq!(parse_ipv6_conntrack(&broken), ParsedCtPacket::Invalid);
    }

    #[test]
    fn ah_and_atomic_fragment_parse_but_real_fragments_need_defrag() {
        let payload = udp();
        let mut ah = Vec::from([17, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        ah.extend_from_slice(&payload);
        assert!(matches!(
            parse_ipv6_conntrack(&ip(51, SRC, DST, &ah)),
            ParsedCtPacket::Flow { .. }
        ));
        let mut frag = Vec::from([17, 0, 0, 0, 0, 0, 0, 1]);
        frag.extend_from_slice(&payload);
        let mut packet = ip(44, SRC, DST, &frag);
        assert!(matches!(
            parse_ipv6_conntrack(&packet),
            ParsedCtPacket::Flow { .. }
        ));
        packet[43] = 1; // More fragments follow.
        assert_eq!(parse_ipv6_conntrack(&packet), ParsedCtPacket::Invalid);
        packet[42..44].copy_from_slice(&8u16.to_be_bytes()); // Non-first fragment.
        assert_eq!(parse_ipv6_conntrack(&packet), ParsedCtPacket::Invalid);
        assert_eq!(
            parse_ipv6_conntrack(&ip(59, SRC, DST, &[])),
            ParsedCtPacket::Invalid
        );
        assert_eq!(
            parse_ipv6_conntrack(&ip(50, SRC, DST, &[0; 8])),
            ParsedCtPacket::Flow {
                tuple: CtTuple {
                    src: SRC.into(),
                    dst: DST.into(),
                    l4: CtL4::Generic { protocol: 50 },
                },
                kind: CtPacketKind::Generic,
            }
        );
    }

    #[test]
    fn icmpv6_query_types_track_but_ndp_and_mld_do_not() {
        let mut echo = icmp(128, SRC, DST, &[]);
        echo[4..6].copy_from_slice(&42u16.to_be_bytes());
        echo[2..4].fill(0);
        fill_checksum(&mut echo, 2, 58, SRC, DST);
        let ParsedCtPacket::Flow {
            tuple,
            kind: CtPacketKind::IcmpQuery,
        } = parse_ipv6_conntrack(&ip(58, SRC, DST, &echo))
        else {
            panic!("echo request must track")
        };
        assert_eq!(
            tuple.l4,
            CtL4::Icmpv6 {
                identifier: 42,
                kind: 128,
                code: 0
            }
        );
        assert_eq!(
            tuple.reverse().unwrap().l4,
            CtL4::Icmpv6 {
                identifier: 42,
                kind: 129,
                code: 0
            }
        );
        let mut ni = icmp(139, SRC, DST, &[1, 2, 3, 4, 5, 6, 7, 8]);
        ni[4..6].copy_from_slice(&2u16.to_be_bytes());
        ni[2..4].fill(0);
        fill_checksum(&mut ni, 2, 58, SRC, DST);
        assert!(matches!(
            parse_ipv6_conntrack(&ip(58, SRC, DST, &ni)),
            ParsedCtPacket::Flow {
                tuple: CtTuple {
                    l4: CtL4::Icmpv6 {
                        identifier: 2,
                        kind: 139,
                        ..
                    },
                    ..
                },
                ..
            }
        ));
        for kind in [130, 133, 135, 136, 143] {
            assert_eq!(
                parse_ipv6_conntrack(&ip(58, SRC, DST, &icmp(kind, SRC, DST, &[]))),
                ParsedCtPacket::Untracked
            );
        }
    }

    #[test]
    fn icmpv6_error_quotes_minimal_udp_and_checks_outer_destination_later() {
        let quoted = ip(17, SRC, DST, &udp());
        let error = icmp(1, DST, SRC, &quoted[..48]);
        let ParsedCtPacket::Related {
            quoted,
            outer_destination,
        } = parse_ipv6_conntrack(&ip(58, DST, SRC, &error))
        else {
            panic!("minimal ICMPv6 quote must parse")
        };
        assert_eq!(quoted.src, SRC.into());
        assert_eq!(quoted.dst, DST.into());
        assert_eq!(outer_destination, SRC.into());
        assert_eq!(
            quoted.l4,
            CtL4::Udp {
                src_port: 1234,
                dst_port: 8080
            }
        );
        let mut truncated = error;
        truncated.pop();
        truncated[2..4].fill(0);
        fill_checksum(&mut truncated, 2, 58, DST, SRC);
        assert_eq!(
            parse_ipv6_conntrack(&ip(58, DST, SRC, &truncated)),
            ParsedCtPacket::Invalid
        );
    }

    #[test]
    fn related_quote_can_contain_first_fragment_but_not_later_fragment() {
        let mut fragment = Vec::from([17, 0, 0, 1, 0, 0, 0, 1]);
        fragment.extend_from_slice(&udp()[..8]);
        let quoted = ip(44, SRC, DST, &fragment);
        let error = icmp(2, DST, SRC, &quoted);
        assert!(matches!(
            parse_ipv6_conntrack(&ip(58, DST, SRC, &error)),
            ParsedCtPacket::Related {
                quoted: CtTuple {
                    l4: CtL4::Udp { .. },
                    ..
                },
                ..
            }
        ));
        fragment[2..4].copy_from_slice(&8u16.to_be_bytes());
        let quoted = ip(44, SRC, DST, &fragment);
        let error = icmp(2, DST, SRC, &quoted);
        assert_eq!(
            parse_ipv6_conntrack(&ip(58, DST, SRC, &error)),
            ParsedCtPacket::Invalid
        );
    }

    #[test]
    fn redirect_header_quote_requires_valid_nd_scope_and_option() {
        let mut router = [0u8; 16];
        router[0] = 0xfe;
        router[1] = 0x80;
        router[15] = 1;
        let quoted = ip(17, SRC, DST, &udp());
        let mut body = Vec::from([0u8; 40]); // Target, destination, option prefix.
        body[32] = 4; // Redirected Header option.
        body[33] = 7; // Eight option bytes plus 48 quoted bytes.
        body.extend_from_slice(&quoted[..48]);
        let redirect = icmp(137, router, SRC, &body);
        let mut packet = ip(58, router, SRC, &redirect);
        packet[7] = 255;
        assert!(matches!(
            parse_ipv6_conntrack(&packet),
            ParsedCtPacket::Related { .. }
        ));
        packet[7] = 64;
        assert_eq!(parse_ipv6_conntrack(&packet), ParsedCtPacket::Invalid);
        packet[7] = 255;
        packet[80] = 2; // First ND option is not Redirected Header.
        assert_eq!(
            parse_ipv6_conntrack_with_mode(&packet, CtChecksumMode::Skip),
            ParsedCtPacket::Untracked
        );
    }

    #[test]
    fn truncated_or_jumbo_headers_do_not_make_partial_keys() {
        let mut packet = ip(17, SRC, DST, &udp());
        packet[4..6].copy_from_slice(&100u16.to_be_bytes());
        assert_eq!(parse_ipv6_conntrack(&packet), ParsedCtPacket::Invalid);
        packet[4..6].copy_from_slice(&0u16.to_be_bytes());
        packet[6] = 0;
        assert_eq!(parse_ipv6_conntrack(&packet), ParsedCtPacket::Invalid);
        packet[0] = 0x40;
        assert_eq!(parse_ipv6_conntrack(&packet), ParsedCtPacket::Invalid);
    }

    #[test]
    fn esp_uses_generic_tuple_but_neighbor_discovery_is_untracked() {
        assert_eq!(
            parse_ipv6_conntrack(&ip(50, SRC, DST, &[1, 2, 3, 4])),
            ParsedCtPacket::Flow {
                tuple: CtTuple {
                    src: SRC.into(),
                    dst: DST.into(),
                    l4: CtL4::Generic { protocol: 50 },
                },
                kind: CtPacketKind::Generic,
            }
        );
        let nd = icmp(135, SRC, DST, &[0; 16]);
        assert_eq!(
            parse_ipv6_conntrack(&ip(58, SRC, DST, &nd)),
            ParsedCtPacket::Untracked
        );
    }
}
