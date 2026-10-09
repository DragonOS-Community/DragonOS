//! Validated ICMP path-MTU feedback. Parsing does not acquire transport locks;
//! protocol association and learning run after the netdevice poll releases them.

use crate::{process::namespace::net_namespace::NetNamespace, time::Instant};
use alloc::{sync::Arc, vec::Vec};
use smoltcp::wire::{IpAddress, IpProtocol, IpVersion, Ipv4Address, Ipv6Address};

#[derive(Debug)]
pub(crate) struct PmtuFeedback {
    pub(crate) source: IpAddress,
    pub(crate) destination: IpAddress,
    pub(crate) protocol: IpProtocol,
    pub(crate) src_port: u16,
    pub(crate) dst_port: u16,
    pub(crate) seq: Option<i32>,
    pub(crate) mtu: u32,
    pub(crate) offender: IpAddress,
    pub(crate) ingress_ifindex: u32,
    pub(crate) quote: Vec<u8>,
    pub(crate) transport_offset: usize,
    pub(crate) outer_ttl: u8,
    pub(crate) outer_tos: u8,
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

pub(crate) fn parse(packet: &[u8], ingress_ifindex: u32) -> Option<PmtuFeedback> {
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
    let (mtu, ttl, tos) = match version {
        IpVersion::Ipv4 => {
            if protocol != IpProtocol::Icmp
                || error[0] != 3
                || error[1] != 4
                || !checksum_valid(checksum_sum(&packet[..offset]))
                || !checksum_valid(checksum_sum(error))
            {
                return None;
            }
            (u32::from(word(error, 6)), packet[8], packet[1])
        }
        IpVersion::Ipv6 => {
            if protocol != IpProtocol::Icmpv6 || error[0] != 2 || error[1] != 0 {
                return None;
            }
            let sum = checksum_sum(&packet[8..40]) + error.len() as u32 + 58 + checksum_sum(error);
            if !checksum_valid(sum) {
                return None;
            }
            (
                u32::from_be_bytes(error[4..8].try_into().ok()?),
                packet[7],
                (packet[0] << 4) | (packet[1] >> 4),
            )
        }
    };
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
    Some(PmtuFeedback {
        source,
        destination,
        protocol,
        src_port,
        dst_port,
        seq,
        mtu,
        offender,
        ingress_ifindex,
        quote: owned,
        transport_offset,
        outer_ttl: ttl,
        outer_tos: tos,
    })
}

pub(crate) fn routed_quote_flow(
    netns: &Arc<NetNamespace>,
    feedback: &PmtuFeedback,
) -> (IpAddress, IpAddress) {
    let tuple = quote_tuple(feedback);
    tuple
        .and_then(|tuple| netns.conntrack().output_tuple(tuple, Instant::now()))
        .map_or((feedback.source, feedback.destination), |tuple| {
            (ip(tuple.src), ip(tuple.dst))
        })
}

fn addr(value: IpAddress) -> super::conntrack::CtAddress {
    match value {
        IpAddress::Ipv4(value) => super::conntrack::CtAddress::V4(value.octets()),
        IpAddress::Ipv6(value) => super::conntrack::CtAddress::V6(value.octets()),
    }
}
fn ip(value: super::conntrack::CtAddress) -> IpAddress {
    match value {
        super::conntrack::CtAddress::V4(value) => v4(&value),
        super::conntrack::CtAddress::V6(value) => v6(&value),
    }
}

pub(crate) fn routed_output_flow(
    netns: &NetNamespace,
    local: smoltcp::wire::IpEndpoint,
    remote: smoltcp::wire::IpEndpoint,
    protocol: IpProtocol,
) -> (IpAddress, IpAddress) {
    use super::conntrack::{CtL4, CtTuple};
    let l4 = match protocol {
        IpProtocol::Tcp => CtL4::Tcp {
            src_port: local.port,
            dst_port: remote.port,
        },
        IpProtocol::Udp => CtL4::Udp {
            src_port: local.port,
            dst_port: remote.port,
        },
        _ => return (local.addr, remote.addr),
    };
    let tuple = CtTuple {
        src: addr(local.addr),
        dst: addr(remote.addr),
        l4,
    };
    netns
        .conntrack()
        .output_tuple(tuple, Instant::now())
        .map_or((local.addr, remote.addr), |tuple| {
            (ip(tuple.src), ip(tuple.dst))
        })
}

pub(crate) fn quote_tuple(feedback: &PmtuFeedback) -> Option<super::conntrack::CtTuple> {
    use super::conntrack::{CtL4, CtTuple};
    let l4 = match feedback.protocol {
        IpProtocol::Tcp => CtL4::Tcp {
            src_port: feedback.src_port,
            dst_port: feedback.dst_port,
        },
        IpProtocol::Udp => CtL4::Udp {
            src_port: feedback.src_port,
            dst_port: feedback.dst_port,
        },
        IpProtocol::Icmp => CtL4::Icmp {
            identifier: word(&feedback.quote, feedback.transport_offset + 4),
            kind: feedback.quote[feedback.transport_offset],
            code: feedback.quote[feedback.transport_offset + 1],
        },
        IpProtocol::Icmpv6 => CtL4::Icmpv6 {
            identifier: word(&feedback.quote, feedback.transport_offset + 4),
            kind: feedback.quote[feedback.transport_offset],
            code: feedback.quote[feedback.transport_offset + 1],
        },
        protocol => CtL4::Generic {
            protocol: protocol.into(),
        },
    };
    Some(CtTuple {
        src: addr(feedback.source),
        dst: addr(feedback.destination),
        l4,
    })
}

pub(crate) fn execute(netns: &Arc<NetNamespace>, feedback: PmtuFeedback) {
    super::socket::inet::raw::handle_pmtu_feedback(netns, &feedback);
    match feedback.protocol {
        IpProtocol::Udp => netns.udp_bindings().handle_pmtu_feedback(&feedback),
        IpProtocol::Tcp => {
            netns.tcp_stack().queue_pmtu_feedback(feedback);
        }
        _ => {}
    }
}
