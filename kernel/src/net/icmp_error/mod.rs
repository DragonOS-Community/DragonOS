//! Validated ICMP error feedback shared by IPv4 and IPv6 transports.
//! Parsing never acquires transport locks. TCP control input is queued at IP
//! admission and delivered by its task; other feedback is deferred until the
//! netdevice poll releases its locks. Protocol-specific policy stays in sockets.

use crate::process::namespace::net_namespace::NetNamespace;
use alloc::{sync::Arc, vec::Vec};
use smoltcp::wire::{IpAddress, IpProtocol, IpVersion, Ipv4Address, Ipv6Address};
use system_error::SystemError;

mod flow;
pub(crate) use flow::{routed_output_flow, routed_quote_flow};

/// MTU updates are explicit: an ordinary transport error must never change a route.
#[derive(Debug, Clone)]
pub(crate) enum IcmpErrorKind {
    Mtu(u32),
    Other {
        errno: SystemError,
        hard: bool,
        info: u32,
    },
}

#[derive(Debug)]
pub struct IcmpErrorFeedback {
    pub(crate) source: IpAddress,
    pub(crate) destination: IpAddress,
    pub(crate) protocol: IpProtocol,
    pub(crate) src_port: u16,
    pub(crate) dst_port: u16,
    pub(crate) seq: Option<i32>,
    pub(crate) kind: IcmpErrorKind,
    pub(crate) icmp_type: u8,
    pub(crate) icmp_code: u8,
    pub(crate) offender: IpAddress,
    pub(crate) ingress_ifindex: u32,
    pub(crate) quote: Vec<u8>,
    pub(crate) transport_offset: usize,
    pub(crate) outer_ttl: u8,
    pub(crate) outer_tos: u8,
}

impl IcmpErrorFeedback {
    pub(crate) fn mtu(&self) -> Option<u32> {
        match &self.kind {
            IcmpErrorKind::Mtu(mtu) => Some(*mtu),
            IcmpErrorKind::Other { .. } => None,
        }
    }

    pub(crate) fn errno(&self) -> SystemError {
        match &self.kind {
            IcmpErrorKind::Mtu(_) => SystemError::EMSGSIZE,
            IcmpErrorKind::Other { errno, .. } => errno.clone(),
        }
    }

    /// Linux's protocol-neutral conversion table. MTU policy is decided by
    /// each transport (in particular IPv4 UDP's IP_MTU_DISCOVER setting).
    pub(crate) fn hard_error(&self) -> bool {
        match &self.kind {
            IcmpErrorKind::Mtu(_) => false,
            IcmpErrorKind::Other { hard, .. } => *hard,
        }
    }

    pub(crate) fn info(&self) -> u32 {
        match &self.kind {
            IcmpErrorKind::Mtu(mtu) => *mtu,
            IcmpErrorKind::Other { info, .. } => *info,
        }
    }
}

/// Linux 6.6 icmp_err_convert / icmpv6_err_convert, independent of sockets.
fn error_kind(version: IpVersion, error: &[u8]) -> Option<IcmpErrorKind> {
    use SystemError::*;
    let error = error.get(..8)?;
    let kind = *error.first()?;
    let code = *error.get(1)?;
    let (errno, hard, info) = match (version, kind) {
        (IpVersion::Ipv4, 3) => {
            let (errno, hard) = match code {
                0 => (ENETUNREACH, false),
                1 => (EHOSTUNREACH, false),
                2 => (ENOPROTOOPT, true),
                3 => (ECONNREFUSED, true),
                4 => return Some(IcmpErrorKind::Mtu(u32::from(word(error, 6)))),
                5 => (EOPNOTSUPP_OR_ENOTSUP, false),
                6 | 9 => (ENETUNREACH, true),
                7 => (EHOSTDOWN, true),
                8 => (ENONET, true),
                10 | 13..=15 => (EHOSTUNREACH, true),
                11 => (ENETUNREACH, false),
                12 => (EHOSTUNREACH, false),
                _ => return None,
            };
            (errno, hard, 0)
        }
        // Reassembly timeout is consumed by IPv4 ICMP, not socket delivery.
        (IpVersion::Ipv4, 11) if code != 1 => (EHOSTUNREACH, false, 0),
        (IpVersion::Ipv4, 12) => (EPROTO, true, u32::from(error[4])),
        (IpVersion::Ipv6, 1) => {
            let (errno, hard) = match code {
                0 => (ENETUNREACH, false),
                1 | 5 | 6 => (EACCES, true),
                2 | 3 => (EHOSTUNREACH, false),
                4 => (ECONNREFUSED, true),
                // Linux intentionally reports unknown unreachable codes too.
                _ => (EPROTO, true),
            };
            (errno, hard, 0)
        }
        (IpVersion::Ipv6, 2) if code == 0 => {
            return Some(IcmpErrorKind::Mtu(u32::from_be_bytes(
                error.get(4..8)?.try_into().ok()?,
            )));
        }
        (IpVersion::Ipv6, 3) => (EHOSTUNREACH, false, 0),
        (IpVersion::Ipv6, 4) => (
            EPROTO,
            true,
            u32::from_be_bytes(error.get(4..8)?.try_into().ok()?),
        ),
        // RFC 4443 / Linux icmpv6_rcv forwards unknown error types to the
        // quoted transport. Informational messages are not errors; malformed
        // codes of known types must not enter this fallback either.
        (IpVersion::Ipv6, kind) if kind < 128 && !matches!(kind, 1..=4) => (
            EPROTO,
            false,
            u32::from_be_bytes(error.get(4..8)?.try_into().ok()?),
        ),
        // Redirects, informational messages and obsolete source-quench do not
        // share transport-error policy or cause a PMTU update.
        _ => return None,
    };
    Some(IcmpErrorKind::Other { errno, hard, info })
}

fn word(bytes: &[u8], at: usize) -> u16 {
    u16::from_be_bytes([bytes[at], bytes[at + 1]])
}

fn checksum_sum(bytes: &[u8]) -> u32 {
    bytes
        .chunks(2)
        .map(|pair| (u32::from(pair[0]) << 8) | u32::from(*pair.get(1).unwrap_or(&0)))
        .sum()
}

fn checksum_valid(mut sum: u32) -> bool {
    while sum > 0xffff {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    sum == 0xffff
}

fn v4(bytes: &[u8]) -> IpAddress {
    IpAddress::Ipv4(Ipv4Address::new(bytes[0], bytes[1], bytes[2], bytes[3]))
}
fn v6(bytes: &[u8]) -> IpAddress {
    IpAddress::Ipv6(Ipv6Address::from(<[u8; 16]>::try_from(bytes).unwrap()))
}

/// Find the transport header of a complete outer packet or truncated quote.
/// A quote's advertised payload length need not fit the received ICMP bytes.
fn transport(
    bytes: &[u8],
    version: IpVersion,
) -> Option<(IpAddress, IpAddress, IpProtocol, usize)> {
    match version {
        IpVersion::Ipv4 => {
            if bytes.len() < 20 || bytes[0] >> 4 != 4 {
                return None;
            }
            let offset = usize::from(bytes[0] & 15) * 4;
            if offset < 20 || offset > bytes.len() || word(bytes, 6) & 0x1fff != 0 {
                return None;
            }
            Some((
                v4(&bytes[12..16]),
                v4(&bytes[16..20]),
                bytes[9].into(),
                offset,
            ))
        }
        IpVersion::Ipv6 => {
            if bytes.len() < 40 || bytes[0] >> 4 != 6 {
                return None;
            }
            let mut next = bytes[6];
            let mut offset = 40;
            // Each supported extension consumes at least eight bytes; the
            // quote length therefore bounds both work and header traversal.
            while matches!(next, 0 | 43 | 44 | 51 | 60) {
                if offset + 2 > bytes.len() {
                    return None;
                }
                let size = match next {
                    44 => 8,
                    51 => (usize::from(bytes[offset + 1]) + 2) * 4,
                    _ => (usize::from(bytes[offset + 1]) + 1) * 8,
                };
                if offset + size > bytes.len() || size < 8 {
                    return None;
                }
                if next == 44 && word(bytes, offset + 2) & 0xfff8 != 0 {
                    return None;
                }
                next = bytes[offset];
                offset += size;
            }
            Some((v6(&bytes[8..24]), v6(&bytes[24..40]), next.into(), offset))
        }
    }
}

pub(crate) fn parse(packet: &[u8], ingress_ifindex: u32) -> Option<IcmpErrorFeedback> {
    // of_packet reads the first byte without checking the slice length.
    let version = IpVersion::of_packet(packet.get(..1)?).ok()?;
    let (offender, outer_destination, protocol, offset) = transport(packet, version)?;
    let total = match version {
        IpVersion::Ipv4 => usize::from(word(packet, 2)),
        IpVersion::Ipv6 => 40 + usize::from(word(packet, 4)),
    };
    if total > packet.len() || offset + 8 > total {
        return None;
    }
    let error = &packet[offset..total];
    let (ttl, tos) = match version {
        IpVersion::Ipv4 => {
            if protocol != IpProtocol::Icmp
                || !checksum_valid(checksum_sum(&packet[..offset]))
                || !checksum_valid(checksum_sum(error))
            {
                return None;
            }
            (packet[8], packet[1])
        }
        IpVersion::Ipv6 => {
            if protocol != IpProtocol::Icmpv6 {
                return None;
            }
            let sum = checksum_sum(&packet[8..40]) + error.len() as u32 + 58 + checksum_sum(error);
            if !checksum_valid(sum) {
                return None;
            }
            (packet[7], (packet[0] << 4) | (packet[1] >> 4))
        }
    };
    let kind = error_kind(version, error)?;
    let quote = &error[8..];
    // A checksummed ICMP error may contain no quote at all. transport checks
    // both the fixed header length and the expected IP version before access.
    let (source, destination, protocol, transport_offset) = transport(quote, version)?;
    if transport_offset + 8 > quote.len() || source != outer_destination {
        return None;
    }
    let (src_port, dst_port, seq) = match protocol {
        IpProtocol::Tcp => (
            word(quote, transport_offset),
            word(quote, transport_offset + 2),
            Some(i32::from_be_bytes(
                quote[transport_offset + 4..transport_offset + 8]
                    .try_into()
                    .ok()?,
            )),
        ),
        IpProtocol::Udp => (
            word(quote, transport_offset),
            word(quote, transport_offset + 2),
            None,
        ),
        _ => (0, 0, None),
    };
    let mut owned = Vec::new();
    // Admission bounds the complete quotation; never silently truncate the
    // user-visible error payload just to make a control event fit its quota.
    if transport_offset + 8 > quote.len() || owned.try_reserve_exact(quote.len()).is_err() {
        return None;
    }
    owned.extend_from_slice(quote);
    Some(IcmpErrorFeedback {
        source,
        destination,
        protocol,
        src_port,
        dst_port,
        seq,
        kind,
        icmp_type: error[0],
        icmp_code: error[1],
        offender,
        ingress_ifindex,
        quote: owned,
        transport_offset,
        outer_ttl: ttl,
        outer_tos: tos,
    })
}

pub(crate) fn execute(netns: &Arc<NetNamespace>, feedback: IcmpErrorFeedback) {
    // TCP feedback must join the segment FIFO at admission, not at the end of
    // the device poll. Its RAW subscribers are served by that same TCP event.
    debug_assert_ne!(feedback.protocol, IpProtocol::Tcp);
    super::socket::inet::raw::handle_icmp_feedback(netns, &feedback);
    if feedback.protocol == IpProtocol::Udp {
        netns.udp_bindings().handle_icmp_feedback(&feedback);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn checksum(sum: u32) -> [u8; 2] {
        let mut sum = sum;
        while sum > 0xffff {
            sum = (sum & 0xffff) + (sum >> 16);
        }
        (!(sum as u16)).to_be_bytes()
    }

    fn packet(version: IpVersion, kind: u8, code: u8, info: u32) -> Vec<u8> {
        let header = match version {
            IpVersion::Ipv4 => 20,
            IpVersion::Ipv6 => 40,
        };
        let mut packet = vec![0; header * 2 + 16];
        let total = packet.len() as u16;
        let quote = header + 8;
        match version {
            IpVersion::Ipv4 => {
                packet[0] = 0x45;
                packet[2..4].copy_from_slice(&total.to_be_bytes());
                packet[8] = 64;
                packet[9] = 1;
                packet[12..16].copy_from_slice(&[192, 0, 2, 2]);
                packet[16..20].copy_from_slice(&[192, 0, 2, 1]);
                packet[quote] = 0x45;
                packet[quote + 2..quote + 4].copy_from_slice(&28u16.to_be_bytes());
                packet[quote + 9] = 17;
                packet[quote + 12..quote + 16].copy_from_slice(&[192, 0, 2, 1]);
                packet[quote + 16..quote + 20].copy_from_slice(&[192, 0, 2, 2]);
            }
            IpVersion::Ipv6 => {
                packet[0] = 0x60;
                packet[4..6].copy_from_slice(&(total - 40).to_be_bytes());
                packet[6] = 58;
                packet[7] = 64;
                packet[8] = 0xfd;
                packet[23] = 2;
                packet[24] = 0xfd;
                packet[39] = 1;
                packet[quote] = 0x60;
                packet[quote + 4..quote + 6].copy_from_slice(&8u16.to_be_bytes());
                packet[quote + 6] = 17;
                packet[quote + 8] = 0xfd;
                packet[quote + 23] = 1;
                packet[quote + 24] = 0xfd;
                packet[quote + 39] = 2;
            }
        }
        packet[header] = kind;
        packet[header + 1] = code;
        packet[header + 4..header + 8].copy_from_slice(&info.to_be_bytes());
        packet[quote + header..quote + header + 2].copy_from_slice(&1234u16.to_be_bytes());
        packet[quote + header + 2..quote + header + 4].copy_from_slice(&4321u16.to_be_bytes());
        reseal(&mut packet, version);
        packet
    }

    fn reseal(packet: &mut [u8], version: IpVersion) {
        let (offset, pseudo) = match version {
            IpVersion::Ipv4 => {
                packet[10..12].fill(0);
                let sum = checksum(checksum_sum(&packet[..20]));
                packet[10..12].copy_from_slice(&sum);
                (20, 0)
            }
            IpVersion::Ipv6 => (
                40,
                checksum_sum(&packet[8..40]) + packet.len() as u32 - 40 + 58,
            ),
        };
        packet[offset + 2..offset + 4].fill(0);
        let sum = checksum(pseudo + checksum_sum(&packet[offset..]));
        packet[offset + 2..offset + 4].copy_from_slice(&sum);
    }

    #[test]
    fn port_unreachable_is_not_mtu_and_preserves_identity() {
        for (version, kind, code) in [(IpVersion::Ipv4, 3, 3), (IpVersion::Ipv6, 1, 4)] {
            let packet = packet(version, kind, code, 0);
            let feedback = parse(&packet, 7).unwrap();
            assert_eq!(feedback.errno(), SystemError::ECONNREFUSED);
            assert!(feedback.hard_error());
            assert_eq!(feedback.mtu(), None);
            assert_eq!(feedback.info(), 0);
            assert_eq!((feedback.icmp_type, feedback.icmp_code), (kind, code));
            assert_eq!((feedback.src_port, feedback.dst_port), (1234, 4321));
            assert_eq!(feedback.ingress_ifindex, 7);
        }
    }

    #[test]
    fn malformed_checksum_truncation_and_noninitial_fragment_are_rejected() {
        for (version, kind, code, fragment_at) in
            [(IpVersion::Ipv4, 3, 3, 34), (IpVersion::Ipv6, 1, 4, 0)]
        {
            let mut packet = packet(version, kind, code, 0);
            for len in 0..packet.len() {
                assert!(parse(&packet[..len], 1).is_none());
            }
            let last = packet.len() - 1;
            packet[last] ^= 1;
            assert!(parse(&packet, 1).is_none());
            packet[last] ^= 1;
            if fragment_at != 0 {
                packet[fragment_at..fragment_at + 2].copy_from_slice(&1u16.to_be_bytes());
                reseal(&mut packet, version);
                assert!(parse(&packet, 1).is_none());
            }
        }
        // Quoted IPv6 fragment offset is nonzero; enough payload remains to
        // fool an unchecked UDP-header parser, but cannot identify a flow.
        let mut quote = vec![0; 56];
        quote[0] = 0x60;
        quote[6] = 44;
        quote[40] = 17;
        quote[42..44].copy_from_slice(&8u16.to_be_bytes());
        assert!(transport(&quote, IpVersion::Ipv6).is_none());
        quote[42..44].fill(0);
        assert_eq!(transport(&quote, IpVersion::Ipv6).unwrap().3, 48);
        assert!(transport(&quote[..47], IpVersion::Ipv6).is_none());
    }

    #[test]
    fn family_specific_codes_and_parameter_pointer_follow_linux() {
        assert!(parse(&packet(IpVersion::Ipv4, 3, 16, 0), 1).is_none());
        assert!(parse(&packet(IpVersion::Ipv4, 11, 1, 0), 1).is_none());
        assert!(parse(&packet(IpVersion::Ipv4, 4, 0, 0), 1).is_none());
        let unknown = parse(&packet(IpVersion::Ipv6, 1, 255, 0), 1).unwrap();
        assert_eq!(unknown.errno(), SystemError::EPROTO);
        assert!(unknown.hard_error());
        for kind in [0, 5, 127] {
            let unknown = parse(&packet(IpVersion::Ipv6, kind, 17, 0x01020304), 1).unwrap();
            assert_eq!(unknown.errno(), SystemError::EPROTO);
            assert!(!unknown.hard_error());
            assert_eq!(unknown.info(), 0x01020304);
            assert_eq!(unknown.mtu(), None);
        }
        assert!(parse(&packet(IpVersion::Ipv6, 128, 0, 0), 1).is_none());
        assert!(parse(&packet(IpVersion::Ipv6, 2, 1, 1280), 1).is_none());
        let ipv4 = parse(&packet(IpVersion::Ipv4, 12, 0, 0x12345678), 1).unwrap();
        let ipv6 = parse(&packet(IpVersion::Ipv6, 4, 0, 0x12345678), 1).unwrap();
        assert_eq!(ipv4.info(), 0x12);
        assert_eq!(ipv6.info(), 0x12345678);
        for error in [ipv4, ipv6] {
            assert_eq!(error.errno(), SystemError::EPROTO);
            assert!(error.hard_error());
            assert_eq!(error.mtu(), None);
        }
    }

    #[test]
    fn only_packet_too_big_and_fragmentation_needed_are_mtu_events() {
        for (version, kind, code) in [(IpVersion::Ipv4, 3, 4), (IpVersion::Ipv6, 2, 0)] {
            let feedback = parse(&packet(version, kind, code, 1280), 1).unwrap();
            assert_eq!(feedback.mtu(), Some(1280));
            assert_eq!(feedback.info(), 1280);
            assert_eq!(feedback.errno(), SystemError::EMSGSIZE);
            assert!(!feedback.hard_error());
        }
        for len in 0..8 {
            assert!(error_kind(IpVersion::Ipv4, &vec![3; len]).is_none());
        }
    }
}
