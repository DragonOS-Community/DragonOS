//! MTU feedback for transit packets.  Work is executed after ingress polling
//! releases the source interface/socket/FIB locks; the generated error enters
//! the ordinary local OUTPUT and POST_ROUTING path.

use alloc::sync::{Arc, Weak};
use core::cmp::min;
use smoltcp::wire::{Icmpv6Packet, IpAddress, IpVersion, Ipv4Packet, Ipv6Packet};
use system_error::SystemError;

use super::conntrack::CtPacketContext;
use crate::{
    driver::net::local_output::reserve_prepared_ip_output,
    process::namespace::net_namespace::NetNamespace,
};

/// Metadata produced only by successful conntrack defragmentation. Linux
/// compares the largest received fragment, not the assembled datagram length,
/// with the egress MTU before allowing a router to re-fragment that datagram.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReassembledForwardInfo {
    pub(crate) max_original_fragment_len: usize,
}

/// Only forwarded datagrams that can require PMTU feedback carry this small
/// sidecar through the existing bounded output queue. The queued frame itself
/// is the phase-correct quote; no second full-packet copy is kept.
#[derive(Clone, Debug)]
pub struct ForwardMtuFeedback {
    netns: Weak<NetNamespace>,
    trigger_context: CtPacketContext,
}

impl ForwardMtuFeedback {
    pub(crate) fn new(
        netns: Arc<NetNamespace>,
        trigger_context: CtPacketContext,
        packet: &[u8],
    ) -> Result<Option<Self>, SystemError> {
        let needed = match IpVersion::of_packet(packet).map_err(|_| SystemError::EINVAL)? {
            IpVersion::Ipv4 => {
                Ipv4Packet::new_checked(packet)
                    .map_err(|_| SystemError::EINVAL)?
                    .dont_frag()
                    && packet.len() > 68
            }
            IpVersion::Ipv6 => packet.len() > 1280,
        };
        if !needed {
            return Ok(None);
        }
        Ok(Some(Self {
            netns: Arc::downgrade(&netns),
            trigger_context,
        }))
    }

    /// Persist the feedback identity only if an output packet has to wait in
    /// the bounded queue. Immediate forwarding needs no per-packet allocation.
    pub(crate) fn persist(&self) -> Result<Arc<Self>, SystemError> {
        Arc::try_new(self.clone()).map_err(|_| SystemError::ENOMEM)
    }

    pub(crate) fn send(&self, trigger: &[u8], mtu: usize) -> Result<(), SystemError> {
        let netns = self.netns.upgrade().ok_or(SystemError::ENODEV)?;
        match IpVersion::of_packet(trigger).map_err(|_| SystemError::EINVAL)? {
            IpVersion::Ipv4 => {
                send_ipv4_frag_needed(&netns, trigger, mtu, Some(&self.trigger_context))
            }
            IpVersion::Ipv6 => {
                if mtu < 1280 {
                    return Err(SystemError::ENETDOWN);
                }
                send_ipv6_packet_too_big(&netns, trigger, mtu, Some(&self.trigger_context))
            }
        }
    }
}

impl ReassembledForwardInfo {
    pub(crate) fn may_refragment(self, mtu: usize) -> bool {
        self.max_original_fragment_len <= mtu
    }
}

fn checksum(bytes: &[u8]) -> u16 {
    let mut sum = 0u32;
    for pair in bytes.chunks(2) {
        sum += (u32::from(pair[0]) << 8) | u32::from(*pair.get(1).unwrap_or(&0));
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

/// Linux's ICMPv4 error exclusions: no error to a non-first fragment,
/// multicast/broadcast destination, invalid source, or another ICMP error.
pub(crate) fn ipv4_error_eligible(bytes: &[u8]) -> bool {
    let Ok(ip) = Ipv4Packet::new_checked(bytes) else {
        return false;
    };
    let source = ip.src_addr();
    let destination = ip.dst_addr();
    if ip.frag_offset() != 0
        || source.is_unspecified()
        || source.is_multicast()
        || source.octets() == [255; 4]
        || destination.is_multicast()
        || destination.octets() == [255; 4]
    {
        return false;
    }
    if bytes[9] == 1 {
        let offset = usize::from(ip.header_len());
        if bytes.len() <= offset {
            return false;
        }
        // Match Linux 6.6 icmp_pointers: reserved error types and unknown
        // types are errors too, rather than recursively provoking feedback.
        if bytes[offset] > 18
            || matches!(bytes[offset], 1 | 2 | 3 | 4 | 5 | 6 | 7 | 9 | 10 | 11 | 12)
        {
            return false;
        }
    }
    true
}

pub(crate) fn send_ipv4_frag_needed(
    netns: &Arc<NetNamespace>,
    trigger: &[u8],
    mtu: usize,
    trigger_context: Option<&CtPacketContext>,
) -> Result<(), SystemError> {
    if !ipv4_error_eligible(trigger) {
        return Ok(());
    }
    let ip = Ipv4Packet::new_checked(trigger).map_err(|_| SystemError::EINVAL)?;
    let destination = IpAddress::Ipv4(ip.src_addr());
    let resolved = super::route::resolve_ipv4_route(netns, destination, None, None)?;
    let route = resolved.output_decision();
    let source = match resolved.source {
        IpAddress::Ipv4(source) => source,
        _ => return Err(SystemError::EAFNOSUPPORT),
    };
    let owner = netns
        .device_list()
        .get(&(route.oif as usize))
        .cloned()
        .ok_or(SystemError::ENETUNREACH)?;
    // Linux caps an ICMPv4 error at 576 bytes. A shorter return route still
    // gets at least the quoted IP header where its minimum MTU permits it.
    let quote_len = min(trigger.len(), min(548, route.ip_mtu.saturating_sub(28)));
    // Linux still emits Frag Needed when the return path only fits a
    // partial IPv4-options header; requiring the complete IHL here would
    // turn a valid PMTU signal into a silent black hole.
    if quote_len < 20 {
        return Err(SystemError::EMSGSIZE);
    }
    let mut output =
        reserve_prepared_ip_output(owner.as_ref(), netns, 28 + quote_len, IpVersion::Ipv4)?;
    let bytes = output.bytes_mut();
    bytes.fill(0);
    bytes[0] = 0x45;
    bytes[2..4].copy_from_slice(&((28 + quote_len) as u16).to_be_bytes());
    bytes[6] = 0x40;
    bytes[8] = 64;
    bytes[9] = 1;
    bytes[12..16].copy_from_slice(&source.octets());
    bytes[16..20].copy_from_slice(&ip.src_addr().octets());
    bytes[20] = 3;
    bytes[21] = 4;
    bytes[26..28].copy_from_slice(&(mtu.min(u16::MAX as usize) as u16).to_be_bytes());
    bytes[28..].copy_from_slice(&trigger[..quote_len]);
    let icmp_checksum = checksum(&bytes[20..]);
    bytes[22..24].copy_from_slice(&icmp_checksum.to_be_bytes());
    let ip_checksum = checksum(&bytes[..20]);
    bytes[10..12].copy_from_slice(&ip_checksum.to_be_bytes());
    let related = trigger_context
        .map(|context| context.related_error_context(crate::time::Instant::now(), trigger))
        .transpose()
        .map_err(|_| SystemError::ENOMEM)?
        .flatten();
    let _ = super::output::submit_prepared_ipv4_with_related(
        netns, output, route, false, false, related,
    )?;
    Ok(())
}

/// A router may report Packet Too Big for a multicast destination, unlike
/// most other ICMPv6 errors. The source must still be a routable unicast and
/// an ICMPv6 error must not cause another ICMPv6 error.
pub(crate) fn ipv6_ptb_eligible(bytes: &[u8]) -> bool {
    let Ok(ip) = Ipv6Packet::new_checked(bytes) else {
        return false;
    };
    let source = ip.src_addr();
    if source.is_unspecified() || source.is_multicast() || source.is_loopback() {
        return false;
    }
    // Walk extension headers before testing the final protocol. Truncated
    // headers are invalid; an unrecognized next-header cannot be assumed to
    // be an ICMP error and remains eligible for PTB.
    let mut next: u8 = ip.next_header().into();
    let mut offset = 40;
    while matches!(next, 0 | 43 | 44 | 51 | 60) {
        if offset + 2 > bytes.len() {
            return false;
        }
        let length = match next {
            44 => 8,
            51 => (usize::from(bytes[offset + 1]) + 2) * 4,
            _ => (usize::from(bytes[offset + 1]) + 1) * 8,
        };
        if offset + length > bytes.len() || length < 8 {
            return false;
        }
        if next == 44 && (u16::from_be_bytes([bytes[offset + 2], bytes[offset + 3]]) & 0xfff8) != 0
        {
            return true;
        }
        next = bytes[offset];
        offset += length;
    }
    next != 58 || (offset < bytes.len() && bytes[offset] >= 128)
}

pub(crate) fn send_ipv6_packet_too_big(
    netns: &Arc<NetNamespace>,
    trigger: &[u8],
    mtu: usize,
    trigger_context: Option<&CtPacketContext>,
) -> Result<(), SystemError> {
    if !ipv6_ptb_eligible(trigger) {
        return Ok(());
    }
    let ip = Ipv6Packet::new_checked(trigger).map_err(|_| SystemError::EINVAL)?;
    let destination = IpAddress::Ipv6(ip.src_addr());
    let resolved = super::route::resolve_ipv6_send_route(netns, destination, None, None)?;
    let route = resolved.decision;
    let quote_len = min(trigger.len(), min(1232, route.ip_mtu.saturating_sub(48)));
    if quote_len < 40 {
        return Err(SystemError::EMSGSIZE);
    }
    let mut output = reserve_prepared_ip_output(
        resolved.source_owner.as_ref(),
        netns,
        48 + quote_len,
        IpVersion::Ipv6,
    )?;
    let bytes = output.bytes_mut();
    bytes.fill(0);
    bytes[0] = 0x60;
    bytes[4..6].copy_from_slice(&((8 + quote_len) as u16).to_be_bytes());
    bytes[6] = 58;
    bytes[7] = 64;
    bytes[8..24].copy_from_slice(&resolved.source.octets());
    bytes[24..40].copy_from_slice(&ip.src_addr().octets());
    bytes[40] = 2;
    bytes[44..48].copy_from_slice(&(mtu.max(1280).min(u32::MAX as usize) as u32).to_be_bytes());
    bytes[48..].copy_from_slice(&trigger[..quote_len]);
    Icmpv6Packet::new_unchecked(&mut bytes[40..]).fill_checksum(&resolved.source, &ip.src_addr());
    let related = trigger_context
        .map(|context| context.related_error_context(crate::time::Instant::now(), trigger))
        .transpose()
        .map_err(|_| SystemError::ENOMEM)?
        .flatten();
    super::output::submit_prepared_ipv6_with_related(netns, output, route, related)
}
