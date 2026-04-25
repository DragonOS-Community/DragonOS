use alloc::vec::Vec;

use smoltcp::wire::{IpAddress, IpProtocol, IpVersion, Ipv4Packet, Ipv6Packet};
use system_error::SystemError;

use crate::net::socket::utils::{IPV4_MIN_HEADER_LEN, IPV6_HEADER_LEN};

use super::constants::ICMPV6_CHECKSUM_OFFSET;

/// IP 包构造参数
pub(super) struct IpPacketParams<'a> {
    pub(super) payload: &'a [u8],
    pub(super) src: IpAddress,
    pub(super) dst: IpAddress,
    pub(super) protocol: IpProtocol,
    pub(super) ttl: u8,
    pub(super) tos: u8,
    pub(super) ipv6_checksum: i32,
}

/// 构造 IPv4 数据包
fn build_ipv4_packet(params: &IpPacketParams) -> Result<Vec<u8>, SystemError> {
    // IPv4 total length is u16.
    if params
        .payload
        .len()
        .checked_add(IPV4_MIN_HEADER_LEN)
        .filter(|v| *v <= u16::MAX as usize)
        .is_none()
    {
        return Err(SystemError::EMSGSIZE);
    }

    let dst = match params.dst {
        IpAddress::Ipv4(v) => v,
        _ => return Err(SystemError::EAFNOSUPPORT),
    };

    let src = match params.src {
        IpAddress::Ipv4(v) => v,
        _ => return Err(SystemError::EAFNOSUPPORT),
    };

    let mut bytes = vec![0u8; IPV4_MIN_HEADER_LEN + params.payload.len()];
    let mut pkt = Ipv4Packet::new_unchecked(&mut bytes);
    pkt.set_version(4);
    pkt.set_header_len(IPV4_MIN_HEADER_LEN as u8);
    pkt.set_total_len((IPV4_MIN_HEADER_LEN + params.payload.len()) as u16);
    pkt.set_ident(0);
    pkt.clear_flags();
    pkt.set_frag_offset(0);
    pkt.set_hop_limit(params.ttl);
    pkt.set_next_header(params.protocol);
    pkt.set_src_addr(src);
    pkt.set_dst_addr(dst);
    pkt.set_dscp(params.tos >> 2);
    pkt.set_ecn(params.tos & 0x3);
    pkt.payload_mut()[..params.payload.len()].copy_from_slice(params.payload);
    pkt.fill_checksum();
    Ok(bytes)
}

/// 构造 IPv6 数据包
fn build_ipv6_packet(params: &IpPacketParams) -> Result<Vec<u8>, SystemError> {
    // IPv6 payload length is u16; reject jumbograms.
    if params.payload.len() > u16::MAX as usize {
        return Err(SystemError::EMSGSIZE);
    }

    let dst = match params.dst {
        IpAddress::Ipv6(v) => v,
        _ => return Err(SystemError::EAFNOSUPPORT),
    };

    let src = match params.src {
        IpAddress::Ipv6(v) => v,
        _ => return Err(SystemError::EAFNOSUPPORT),
    };

    let mut bytes = vec![0u8; IPV6_HEADER_LEN + params.payload.len()];
    let mut pkt = Ipv6Packet::new_unchecked(&mut bytes);
    pkt.set_version(6);
    pkt.set_traffic_class(params.tos);
    pkt.set_flow_label(0);
    pkt.set_payload_len(params.payload.len() as u16);
    pkt.set_next_header(params.protocol);
    pkt.set_hop_limit(params.ttl);
    pkt.set_src_addr(src);
    pkt.set_dst_addr(dst);
    pkt.payload_mut()[..params.payload.len()].copy_from_slice(params.payload);

    // ICMPv6：校验和必须存在且由内核计算。
    if params.protocol == IpProtocol::Icmpv6 {
        let off = ICMPV6_CHECKSUM_OFFSET as usize;
        if off + 2 > params.payload.len() {
            return Err(SystemError::EINVAL);
        }
        let xsum = ipv6_icmpv6_checksum(&bytes, off).ok_or(SystemError::EINVAL)?;
        let payload = &mut bytes[IPV6_HEADER_LEN..];
        payload[off..off + 2].copy_from_slice(&xsum.to_be_bytes());
        return Ok(bytes);
    }

    // Linux 语义：当设置 IPV6_CHECKSUM 且协议为 UDP 时，内核负责计算并填充校验和。
    if params.protocol == IpProtocol::Udp && params.ipv6_checksum >= 0 {
        let off = params.ipv6_checksum as usize;
        if !off.is_multiple_of(2) || off + 2 > params.payload.len() {
            return Err(SystemError::EINVAL);
        }
        let xsum = ipv6_udp_checksum(&bytes, off).ok_or(SystemError::EINVAL)?;
        let payload = &mut bytes[IPV6_HEADER_LEN..];
        payload[off..off + 2].copy_from_slice(&xsum.to_be_bytes());
    }
    Ok(bytes)
}

/// 构造 IP 数据包（根据 IP 版本自动选择）
pub(super) fn build_ip_packet(
    ip_version: IpVersion,
    params: &IpPacketParams,
) -> Result<Vec<u8>, SystemError> {
    match ip_version {
        IpVersion::Ipv4 => build_ipv4_packet(params),
        IpVersion::Ipv6 => build_ipv6_packet(params),
    }
}

fn checksum_add(sum: &mut u32, data: &[u8]) {
    let mut i = 0usize;
    while i + 1 < data.len() {
        *sum = sum.wrapping_add(u16::from_be_bytes([data[i], data[i + 1]]) as u32);
        i += 2;
    }
    if i < data.len() {
        *sum = sum.wrapping_add(u16::from_be_bytes([data[i], 0]) as u32);
    }
}

fn checksum_finish(mut sum: u32) -> u16 {
    while (sum >> 16) != 0 {
        sum = (sum & 0xffff).wrapping_add(sum >> 16);
    }
    let out = !(sum as u16);
    if out == 0 {
        0xffff
    } else {
        out
    }
}

pub(super) fn ipv6_udp_checksum(packet: &[u8], checksum_off_in_payload: usize) -> Option<u16> {
    ipv6_upperlayer_checksum(packet, 17, checksum_off_in_payload)
}

pub(super) fn ipv6_icmpv6_checksum(packet: &[u8], checksum_off_in_payload: usize) -> Option<u16> {
    ipv6_upperlayer_checksum(packet, 58, checksum_off_in_payload)
}

fn ipv6_upperlayer_checksum(
    packet: &[u8],
    next_header: u8,
    checksum_off_in_payload: usize,
) -> Option<u16> {
    if packet.len() < IPV6_HEADER_LEN {
        return None;
    }
    let payload = &packet[IPV6_HEADER_LEN..];
    if checksum_off_in_payload + 2 > payload.len() {
        return None;
    }
    if !checksum_off_in_payload.is_multiple_of(2) {
        return None;
    }

    let src = &packet[8..24];
    let dst = &packet[24..40];
    let payload_len = payload.len() as u32;

    let mut sum: u32 = 0;
    checksum_add(&mut sum, src);
    checksum_add(&mut sum, dst);
    checksum_add(&mut sum, &payload_len.to_be_bytes());
    checksum_add(&mut sum, &[0, 0, 0, next_header]);

    // Upper-layer packet with checksum field zeroed.
    checksum_add(&mut sum, &payload[..checksum_off_in_payload]);
    checksum_add(&mut sum, &[0, 0]);
    checksum_add(&mut sum, &payload[checksum_off_in_payload + 2..]);
    Some(checksum_finish(sum))
}

#[inline]
pub(super) fn checksum16(data: &[u8]) -> u16 {
    let mut sum: u32 = 0;
    checksum_add(&mut sum, data);
    checksum_finish(sum)
}
