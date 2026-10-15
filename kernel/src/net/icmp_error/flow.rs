//! Route/conntrack helpers for associating validated ICMP quotations.

use crate::{
    net::icmp_error::IcmpErrorFeedback, process::namespace::net_namespace::NetNamespace,
    time::Instant,
};
use alloc::sync::Arc;
use smoltcp::wire::{IpAddress, IpProtocol, Ipv4Address, Ipv6Address};

fn word(bytes: &[u8], at: usize) -> u16 {
    u16::from_be_bytes([bytes[at], bytes[at + 1]])
}
fn v4(bytes: &[u8]) -> IpAddress {
    IpAddress::Ipv4(Ipv4Address::new(bytes[0], bytes[1], bytes[2], bytes[3]))
}
fn v6(bytes: &[u8]) -> IpAddress {
    IpAddress::Ipv6(Ipv6Address::from(<[u8; 16]>::try_from(bytes).unwrap()))
}

pub(crate) fn routed_quote_flow(
    netns: &Arc<NetNamespace>,
    feedback: &IcmpErrorFeedback,
) -> (IpAddress, IpAddress) {
    let tuple = quote_tuple(feedback);
    tuple
        .and_then(|tuple| netns.conntrack().output_tuple(tuple, Instant::now()))
        .map_or((feedback.source, feedback.destination), |tuple| {
            (ip(tuple.src), ip(tuple.dst))
        })
}

fn addr(value: IpAddress) -> crate::net::conntrack::CtAddress {
    match value {
        IpAddress::Ipv4(value) => crate::net::conntrack::CtAddress::V4(value.octets()),
        IpAddress::Ipv6(value) => crate::net::conntrack::CtAddress::V6(value.octets()),
    }
}
fn ip(value: crate::net::conntrack::CtAddress) -> IpAddress {
    match value {
        crate::net::conntrack::CtAddress::V4(value) => v4(&value),
        crate::net::conntrack::CtAddress::V6(value) => v6(&value),
    }
}

pub(crate) fn routed_output_flow(
    netns: &NetNamespace,
    local: smoltcp::wire::IpEndpoint,
    remote: smoltcp::wire::IpEndpoint,
    protocol: IpProtocol,
) -> (IpAddress, IpAddress) {
    use crate::net::conntrack::{CtL4, CtTuple};
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

fn quote_tuple(feedback: &IcmpErrorFeedback) -> Option<crate::net::conntrack::CtTuple> {
    use crate::net::conntrack::{CtL4, CtTuple};
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
