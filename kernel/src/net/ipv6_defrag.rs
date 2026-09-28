//! Bounded IPv6 reassembly for a namespace's early Netfilter defrag step.
//!
//! This is deliberately independent of routing and packet delivery. The caller
//! serializes access and owns the result's reinjection at the hook after defrag.
//! In particular, a fragment submitted here must not also enter the ordinary
//! transport reassembler.

extern crate alloc;

use alloc::vec::Vec;
use core::mem::size_of;

const IPV6_HEADER_LEN: usize = 40;
const FRAGMENT_HEADER_LEN: usize = 8;
const MAX_PAYLOAD_LEN: usize = u16::MAX as usize;
const MAX_QUEUES: usize = 128;
const MAX_FRAGMENTS_PER_QUEUE: usize = 8192;
const MAX_STORED_BYTES: usize = 4 * 1024 * 1024;
const REASSEMBLY_TIMEOUT_US: u64 = 30_000_000;

// Linux 6.6 include/net/ipv6_frag.h reserves one u16 zone range per user.
const CT_IN_USER: u32 = 1;
const CT_OUT_USER: u32 = CT_IN_USER + u16::MAX as u32 + 1;
const LOCAL_IN_USER: u32 = CT_OUT_USER + u16::MAX as u32 + 1;

/// The namespace is implicit in its owning reassembler. `ifindex` is part of
/// the key only for link-local and multicast destinations, as on Linux.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DefragDomain {
    user: u32,
    ifindex: u32,
}

impl DefragDomain {
    pub(crate) const fn pre_routing(zone: u16, ifindex: u32) -> Self {
        Self {
            user: CT_IN_USER + zone as u32,
            ifindex,
        }
    }

    pub(crate) const fn local_out(zone: u16, ifindex: u32) -> Self {
        Self {
            user: CT_OUT_USER + zone as u32,
            ifindex,
        }
    }

    pub(crate) const fn local_input(zone: u16, ifindex: u32) -> Self {
        Self {
            user: LOCAL_IN_USER + zone as u32,
            ifindex,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Key {
    source: [u8; 16],
    destination: [u8; 16],
    identification: u32,
    user: u32,
    scoped_ifindex: u32,
}

struct Fragment {
    offset: usize,
    payload: Vec<u8>,
}

impl Fragment {
    fn end(&self) -> usize {
        self.offset + self.payload.len()
    }
}

struct Queue<M> {
    key: Key,
    expires_at_us: u64,
    fragments: Vec<Fragment>,
    // Linux reassembles with the unfragmentable header chain from offset
    // zero, even if another fragment arrived first with different headers.
    prefix: Option<Vec<u8>>,
    previous_next_header_offset: Option<usize>,
    fragment_next_header: Option<u8>,
    first_base: Option<[u8; IPV6_HEADER_LEN]>,
    first_origin: Option<M>,
    final_end: Option<usize>,
    largest_end: usize,
    received_bytes: usize,
    ecn_seen: u8,
    charged_bytes: usize,
}

pub(crate) struct ReassembledIpv6<M> {
    pub(crate) packet: Vec<u8>,
    pub(crate) first_origin: M,
    pub(crate) completion_origin: M,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ReassemblyError {
    Malformed,
    Overlap,
    InconsistentLength,
    InconsistentEcn,
    ResourceLimit,
    OutOfMemory,
}

pub(crate) enum ReassemblyResult<M> {
    NotFragment,
    Pending,
    Duplicate,
    Complete(ReassembledIpv6<M>),
    Rejected(ReassemblyError),
}

/// Caller-owned and caller-serialized; no FIB, device, socket or rule locks
/// are acquired while inserting a fragment.
pub(crate) struct Ipv6Defragmenter<M> {
    queues: Vec<Queue<M>>,
    charged_bytes: usize,
}

struct Parsed<'a> {
    key: Key,
    base: [u8; IPV6_HEADER_LEN],
    prefix: &'a [u8],
    previous_next_header_offset: usize,
    fragment_next_header: u8,
    payload: &'a [u8],
    offset: usize,
    more: bool,
    ecn: u8,
    malformed: bool,
}

impl Parsed<'_> {
    fn parse<'a>(
        bytes: &'a [u8],
        domain: DefragDomain,
    ) -> Result<Option<Parsed<'a>>, ReassemblyError> {
        if bytes.len() < IPV6_HEADER_LEN || bytes[0] >> 4 != 6 {
            return Err(ReassemblyError::Malformed);
        }
        let payload_len = usize::from(u16::from_be_bytes([bytes[4], bytes[5]]));
        // Linux's nf_ct_frag6_gather() does not attempt defragmentation for
        // zero Payload Length (including jumbograms). The caller's ordinary
        // IPv6 validation remains responsible for such packets.
        if payload_len == 0 {
            return Ok(None);
        }
        let packet_len = IPV6_HEADER_LEN
            .checked_add(payload_len)
            .ok_or(ReassemblyError::Malformed)?;
        if packet_len > bytes.len() {
            return Err(ReassemblyError::Malformed);
        }
        let packet = &bytes[..packet_len];
        let mut next_header = packet[6];
        let mut previous_next_header_offset = 6;
        let mut offset = IPV6_HEADER_LEN;
        loop {
            match next_header {
                44 => break,
                // Hop-by-hop, Routing, Destination Options, Authentication.
                0 | 43 | 60 | 51 => {
                    if packet_len - offset < 2 {
                        return Err(ReassemblyError::Malformed);
                    }
                    let extension_len = if next_header == 51 {
                        (usize::from(packet[offset + 1]) + 2) * 4
                    } else {
                        (usize::from(packet[offset + 1]) + 1) * 8
                    };
                    if extension_len < 8 || extension_len > packet_len - offset {
                        return Err(ReassemblyError::Malformed);
                    }
                    previous_next_header_offset = offset;
                    next_header = packet[offset];
                    offset += extension_len;
                }
                _ => return Ok(None),
            }
        }
        if packet_len - offset < FRAGMENT_HEADER_LEN {
            return Err(ReassemblyError::Malformed);
        }
        let fragment_next_header = packet[offset];
        let flags_offset = u16::from_be_bytes([packet[offset + 2], packet[offset + 3]]);
        let fragment_offset = usize::from(flags_offset & 0xfff8);
        let more = flags_offset & 1 != 0;
        let payload = &packet[offset + FRAGMENT_HEADER_LEN..];
        let prefix = &packet[IPV6_HEADER_LEN..offset];
        let malformed = payload.is_empty()
            || (more && !payload.len().is_multiple_of(8))
            || fragment_offset
                .checked_add(payload.len())
                .is_none_or(|end| end > MAX_PAYLOAD_LEN)
            || (fragment_offset == 0
                && !first_transport_header_complete(fragment_next_header, payload));
        // RFC 8200 §4.5 and Linux ipv6frag_thdr_truncated(): fragment zero
        // must contain the complete upper-layer header, not just its prefix.
        let source: [u8; 16] = packet[8..24].try_into().unwrap();
        let destination: [u8; 16] = packet[24..40].try_into().unwrap();
        let scoped =
            destination[0] == 0xff || (destination[0] == 0xfe && destination[1] & 0xc0 == 0x80);
        Ok(Some(Parsed {
            key: Key {
                source,
                destination,
                identification: u32::from_be_bytes(
                    packet[offset + 4..offset + 8].try_into().unwrap(),
                ),
                user: domain.user,
                scoped_ifindex: if scoped { domain.ifindex } else { 0 },
            },
            base: packet[..IPV6_HEADER_LEN].try_into().unwrap(),
            prefix,
            previous_next_header_offset,
            fragment_next_header,
            payload,
            offset: fragment_offset,
            more,
            ecn: (packet[1] >> 4) & 3,
            malformed,
        }))
    }
}

/// Inspect a validated receive packet without allocating. The caller uses
/// this before transferring only actual fragments into the bounded worker
/// ring; malformed fragment headers fail closed instead of bypassing defrag.
pub(crate) fn fragment_offset(
    bytes: &[u8],
    domain: DefragDomain,
) -> Result<Option<usize>, ReassemblyError> {
    match Parsed::parse(bytes, domain)? {
        Some(parsed) if parsed.malformed => Err(ReassemblyError::Malformed),
        Some(parsed) => Ok(Some(parsed.offset)),
        None => Ok(None),
    }
}

// The extension headers after the Fragment Header belong to the first
// fragment; walk them before checking the minimum transport header size.
fn first_transport_header_complete(mut next: u8, bytes: &[u8]) -> bool {
    let mut offset = 0;
    loop {
        match next {
            0 | 43 | 60 | 51 => {
                if bytes.len() - offset < 2 {
                    return false;
                }
                let len = if next == 51 {
                    (usize::from(bytes[offset + 1]) + 2) * 4
                } else {
                    (usize::from(bytes[offset + 1]) + 1) * 8
                };
                if len < 8 || len > bytes.len() - offset {
                    return false;
                }
                next = bytes[offset];
                offset += len;
            }
            // Linux checks fixed minimum sizes for TCP, UDP, ICMPv6 and one
            // byte for other upper-layer protocols.
            6 => return bytes.len() - offset >= 20,
            17 => return bytes.len() - offset >= 8,
            58 => return bytes.len() - offset >= 8,
            59 => return true,
            _ => return bytes.len() - offset >= 1,
        }
    }
}

impl<M: Clone> Ipv6Defragmenter<M> {
    pub(crate) const fn new() -> Self {
        Self {
            queues: Vec::new(),
            charged_bytes: 0,
        }
    }

    pub(crate) fn next_deadline_us(&self) -> Option<u64> {
        self.queues.iter().map(|queue| queue.expires_at_us).min()
    }

    pub(crate) fn expire(&mut self, now_us: u64) -> usize {
        let mut expired = 0;
        let mut index = 0;
        while index < self.queues.len() {
            if self.queues[index].expires_at_us <= now_us {
                self.remove(index);
                expired += 1;
            } else {
                index += 1;
            }
        }
        expired
    }

    pub(crate) fn submit(
        &mut self,
        bytes: &[u8],
        domain: DefragDomain,
        origin: M,
        now_us: u64,
    ) -> ReassemblyResult<M> {
        self.expire(now_us);
        let parsed = match Parsed::parse(bytes, domain) {
            Ok(Some(parsed)) => parsed,
            Ok(None) => return ReassemblyResult::NotFragment,
            Err(error) => return ReassemblyResult::Rejected(error),
        };
        // Once the fragment key is known, malformed data must not leave a
        // partially received datagram alive under that same key.
        if parsed.malformed {
            if let Some(index) = self.queues.iter().position(|queue| queue.key == parsed.key) {
                self.remove(index);
            }
            return ReassemblyResult::Rejected(ReassemblyError::Malformed);
        }
        if parsed.offset == 0 && !parsed.more {
            // Atomic fragments do not create or join a queue. Removing the
            // Fragment Header is still required for downstream matching.
            return match assemble_atomic(&parsed, origin) {
                Ok(packet) => ReassemblyResult::Complete(packet),
                Err(error) => ReassemblyResult::Rejected(error),
            };
        }

        let mut index = match self.queues.iter().position(|queue| queue.key == parsed.key) {
            Some(index) => index,
            None => {
                if self.queues.len() == MAX_QUEUES {
                    self.evict_oldest(None);
                }
                if self.queues.try_reserve(1).is_err() {
                    return ReassemblyResult::Rejected(ReassemblyError::OutOfMemory);
                }
                let charge = size_of::<Queue<M>>();
                self.evict_until_room(None, charge);
                if self.charged_bytes.saturating_add(charge) > MAX_STORED_BYTES {
                    return ReassemblyResult::Rejected(ReassemblyError::ResourceLimit);
                }
                self.queues.push(Queue {
                    key: parsed.key,
                    expires_at_us: now_us.saturating_add(REASSEMBLY_TIMEOUT_US),
                    fragments: Vec::new(),
                    prefix: None,
                    previous_next_header_offset: None,
                    fragment_next_header: None,
                    first_base: None,
                    first_origin: None,
                    final_end: None,
                    largest_end: 0,
                    received_bytes: 0,
                    ecn_seen: 0,
                    charged_bytes: charge,
                });
                self.charged_bytes += charge;
                self.queues.len() - 1
            }
        };

        let end = parsed.offset + parsed.payload.len();
        let queue = &self.queues[index];
        if queue.final_end.is_some_and(|final_end| end > final_end)
            || (!parsed.more
                && (queue.largest_end > end
                    || queue.final_end.is_some_and(|final_end| final_end != end)))
        {
            self.remove(index);
            return ReassemblyResult::Rejected(ReassemblyError::InconsistentLength);
        }
        // Linux records the final datagram length before classifying a
        // duplicate range. Such a duplicate does not complete reassembly,
        // but its final-length metadata remains on the queue.
        if !parsed.more {
            self.queues[index].final_end = Some(end);
        }

        // Linux inet_frag_queue_insert ignores a fully covered duplicate but
        // kills the entire datagram on any partial overlap (RFC 5722).
        let queue = &self.queues[index];
        let first_overlap = queue
            .fragments
            .partition_point(|fragment| fragment.end() <= parsed.offset);
        let mut covered = parsed.offset;
        let mut overlaps = false;
        for fragment in &queue.fragments[first_overlap..] {
            if fragment.offset >= end {
                break;
            }
            overlaps = true;
            if fragment.offset > covered {
                break;
            }
            covered = covered.max(fragment.end());
            if covered >= end {
                return ReassemblyResult::Duplicate;
            }
        }
        if overlaps {
            self.remove(index);
            return ReassemblyResult::Rejected(ReassemblyError::Overlap);
        }
        if queue.fragments.len() >= MAX_FRAGMENTS_PER_QUEUE {
            self.remove(index);
            return ReassemblyResult::Rejected(ReassemblyError::ResourceLimit);
        }

        let mut first_prefix = if parsed.offset == 0 {
            let mut prefix = Vec::new();
            if prefix.try_reserve_exact(parsed.prefix.len()).is_err() {
                self.remove(index);
                return ReassemblyResult::Rejected(ReassemblyError::OutOfMemory);
            }
            prefix.extend_from_slice(parsed.prefix);
            Some(prefix)
        } else {
            None
        };
        let mut payload = Vec::new();
        if payload.try_reserve_exact(parsed.payload.len()).is_err() {
            self.remove(index);
            return ReassemblyResult::Rejected(ReassemblyError::OutOfMemory);
        }
        payload.extend_from_slice(parsed.payload);
        let old_capacity = self.queues[index].fragments.capacity();
        if self.queues[index].fragments.try_reserve(1).is_err() {
            self.remove(index);
            return ReassemblyResult::Rejected(ReassemblyError::OutOfMemory);
        }
        let charge = payload
            .capacity()
            .saturating_add(first_prefix.as_ref().map_or(0, Vec::capacity))
            .saturating_add(
                (self.queues[index].fragments.capacity() - old_capacity)
                    .saturating_mul(size_of::<Fragment>()),
            );
        self.evict_until_room(Some(&mut index), charge);
        if self.charged_bytes.saturating_add(charge) > MAX_STORED_BYTES {
            self.remove(index);
            return ReassemblyResult::Rejected(ReassemblyError::ResourceLimit);
        }
        let queue = &mut self.queues[index];
        queue.charged_bytes += charge;
        self.charged_bytes += charge;
        queue.received_bytes += payload.len();
        queue.largest_end = queue.largest_end.max(end);
        queue.ecn_seen |= 1 << parsed.ecn;
        if parsed.offset == 0 {
            queue.prefix = first_prefix.take();
            queue.previous_next_header_offset = Some(parsed.previous_next_header_offset);
            queue.fragment_next_header = Some(parsed.fragment_next_header);
            queue.first_base = Some(parsed.base);
            queue.first_origin = Some(origin.clone());
        }
        let insert_at = queue
            .fragments
            .partition_point(|fragment| fragment.offset < parsed.offset);
        queue.fragments.insert(
            insert_at,
            Fragment {
                offset: parsed.offset,
                payload,
            },
        );

        let Some(final_end) = queue.final_end else {
            return ReassemblyResult::Pending;
        };
        if queue.first_base.is_none() || queue.received_bytes != final_end {
            return ReassemblyResult::Pending;
        }
        let queue = self.remove(index);
        match assemble(queue, origin) {
            Ok(packet) => ReassemblyResult::Complete(packet),
            Err(error) => ReassemblyResult::Rejected(error),
        }
    }

    fn remove(&mut self, index: usize) -> Queue<M> {
        let queue = self.queues.remove(index);
        self.charged_bytes -= queue.charged_bytes;
        queue
    }

    fn evict_oldest(&mut self, except: Option<usize>) -> Option<usize> {
        let index = self
            .queues
            .iter()
            .enumerate()
            .filter(|(index, _)| Some(*index) != except)
            .min_by_key(|(_, queue)| queue.expires_at_us)
            .map(|(index, _)| index)?;
        self.remove(index);
        Some(index)
    }

    fn evict_until_room(&mut self, mut keep: Option<&mut usize>, additional: usize) {
        while self.charged_bytes.saturating_add(additional) > MAX_STORED_BYTES {
            let except = keep.as_ref().map(|index| **index);
            let Some(removed) = self.evict_oldest(except) else {
                break;
            };
            if let Some(index) = keep.as_mut() {
                if removed < **index {
                    **index -= 1;
                }
            }
        }
    }
}

fn assemble_atomic<M: Clone>(
    parsed: &Parsed<'_>,
    origin: M,
) -> Result<ReassembledIpv6<M>, ReassemblyError> {
    let mut packet = Vec::new();
    packet
        .try_reserve_exact(IPV6_HEADER_LEN + parsed.prefix.len() + parsed.payload.len())
        .map_err(|_| ReassemblyError::OutOfMemory)?;
    packet.extend_from_slice(&parsed.base);
    packet.extend_from_slice(parsed.prefix);
    packet.extend_from_slice(parsed.payload);
    finish_header(
        &mut packet,
        parsed.previous_next_header_offset,
        parsed.fragment_next_header,
        0,
    )?;
    Ok(ReassembledIpv6 {
        packet,
        first_origin: origin.clone(),
        completion_origin: origin,
    })
}

fn assemble<M>(
    queue: Queue<M>,
    completion_origin: M,
) -> Result<ReassembledIpv6<M>, ReassemblyError> {
    let base = queue.first_base.ok_or(ReassemblyError::Malformed)?;
    let first_origin = queue.first_origin.ok_or(ReassemblyError::Malformed)?;
    let final_end = queue.final_end.ok_or(ReassemblyError::Malformed)?;
    let prefix = queue.prefix.ok_or(ReassemblyError::Malformed)?;
    let previous_next_header_offset = queue
        .previous_next_header_offset
        .ok_or(ReassemblyError::Malformed)?;
    let fragment_next_header = queue
        .fragment_next_header
        .ok_or(ReassemblyError::Malformed)?;
    if prefix.len().saturating_add(final_end) > MAX_PAYLOAD_LEN {
        return Err(ReassemblyError::InconsistentLength);
    }
    let mut packet = Vec::new();
    packet
        .try_reserve_exact(IPV6_HEADER_LEN + prefix.len() + final_end)
        .map_err(|_| ReassemblyError::OutOfMemory)?;
    packet.extend_from_slice(&base);
    packet.extend_from_slice(&prefix);
    let mut next_offset = 0;
    for fragment in queue.fragments {
        if fragment.offset != next_offset {
            return Err(ReassemblyError::Malformed);
        }
        next_offset += fragment.payload.len();
        packet.extend_from_slice(&fragment.payload);
    }
    if next_offset != final_end {
        return Err(ReassemblyError::Malformed);
    }
    if queue.ecn_seen & 1 != 0 && queue.ecn_seen & !1 != 0 {
        return Err(ReassemblyError::InconsistentEcn);
    }
    let new_ecn = if queue.ecn_seen & (1 << 3) != 0 { 3 } else { 0 };
    finish_header(
        &mut packet,
        previous_next_header_offset,
        fragment_next_header,
        new_ecn,
    )?;
    Ok(ReassembledIpv6 {
        packet,
        first_origin,
        completion_origin,
    })
}

fn finish_header(
    packet: &mut [u8],
    previous_next_header_offset: usize,
    fragment_next_header: u8,
    new_ecn: u8,
) -> Result<(), ReassemblyError> {
    let payload_len = packet.len() - IPV6_HEADER_LEN;
    if payload_len > MAX_PAYLOAD_LEN || previous_next_header_offset >= packet.len() {
        return Err(ReassemblyError::InconsistentLength);
    }
    packet[4..6].copy_from_slice(&(payload_len as u16).to_be_bytes());
    packet[previous_next_header_offset] = fragment_next_header;
    if new_ecn == 3 {
        packet[1] = (packet[1] & !0x30) | 0x30;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    const DOMAIN: DefragDomain = DefragDomain::pre_routing(0, 2);

    fn fragment(
        id: u32,
        offset: usize,
        more: bool,
        payload: &[u8],
        ecn: u8,
        with_extension: bool,
    ) -> Vec<u8> {
        assert!(offset.is_multiple_of(8));
        let prefix_len = if with_extension { 8 } else { 0 };
        let mut bytes = vec![0; IPV6_HEADER_LEN + prefix_len + FRAGMENT_HEADER_LEN + payload.len()];
        bytes[0] = 0x60;
        bytes[1] = ecn << 4;
        bytes[4..6].copy_from_slice(
            &((prefix_len + FRAGMENT_HEADER_LEN + payload.len()) as u16).to_be_bytes(),
        );
        bytes[6] = if with_extension { 60 } else { 44 };
        bytes[7] = 64;
        bytes[8..24].copy_from_slice(&[0x20, 1, 0xd, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
        bytes[24..40].copy_from_slice(&[0x20, 1, 0xd, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2]);
        if with_extension {
            bytes[40] = 44;
        }
        let f = IPV6_HEADER_LEN + prefix_len;
        bytes[f] = 17;
        bytes[f + 2..f + 4].copy_from_slice(&((offset as u16) | u16::from(more)).to_be_bytes());
        bytes[f + 4..f + 8].copy_from_slice(&id.to_be_bytes());
        bytes[f + 8..].copy_from_slice(payload);
        bytes
    }

    fn complete(result: ReassemblyResult<u8>) -> ReassembledIpv6<u8> {
        let ReassemblyResult::Complete(packet) = result else {
            panic!("expected complete IPv6 datagram");
        };
        packet
    }

    #[test]
    fn out_of_order_extension_chain_and_origins() {
        let mut defrag = Ipv6Defragmenter::new();
        let last = fragment(1, 16, false, b"qrstuvwx", 0, true);
        let middle = fragment(1, 8, true, b"ijklmnop", 0, true);
        let first = fragment(1, 0, true, b"abcdefgh", 0, true);
        assert!(matches!(
            defrag.submit(&last, DOMAIN, 3, 0),
            ReassemblyResult::Pending
        ));
        assert!(matches!(
            defrag.submit(&middle, DOMAIN, 4, 1),
            ReassemblyResult::Pending
        ));
        let done = complete(defrag.submit(&first, DOMAIN, 7, 2));
        assert_eq!(done.first_origin, 7);
        assert_eq!(done.completion_origin, 7);
        assert_eq!(done.packet[6], 60);
        assert_eq!(done.packet[40], 17);
        assert_eq!(&done.packet[48..], b"abcdefghijklmnopqrstuvwx");
        assert_eq!(u16::from_be_bytes([done.packet[4], done.packet[5]]), 32);
        assert_eq!(defrag.charged_bytes, 0);
    }

    #[test]
    fn non_copy_first_origin_survives_out_of_order_completion() {
        use alloc::sync::Arc;

        let mut defrag = Ipv6Defragmenter::new();
        let first = fragment(41, 0, true, b"abcdefgh", 0, false);
        let middle = fragment(41, 8, true, b"ijklmnop", 0, false);
        let last = fragment(41, 16, false, b"qrstuvwx", 0, false);
        let first_origin = Arc::new(1u8);
        let last_origin = Arc::new(2u8);
        assert!(matches!(
            defrag.submit(&middle, DOMAIN, Arc::new(3u8), 0),
            ReassemblyResult::Pending
        ));
        assert!(matches!(
            defrag.submit(&first, DOMAIN, first_origin.clone(), 1),
            ReassemblyResult::Pending
        ));
        let ReassemblyResult::Complete(done) = defrag.submit(&last, DOMAIN, last_origin.clone(), 2)
        else {
            panic!("expected reassembled IPv6 datagram");
        };
        assert!(Arc::ptr_eq(&done.first_origin, &first_origin));
        assert!(Arc::ptr_eq(&done.completion_origin, &last_origin));
        assert_eq!(&done.packet[40..], b"abcdefghijklmnopqrstuvwx");
    }

    #[test]
    fn icmpv6_first_fragment_requires_complete_eight_byte_header() {
        assert!(!first_transport_header_complete(58, &[0; 4]));
        assert!(first_transport_header_complete(58, &[0; 8]));
    }

    #[test]
    fn atomic_fragment_is_removed_without_a_queue() {
        let mut defrag = Ipv6Defragmenter::new();
        let atomic = fragment(2, 0, false, b"abcdefgh", 0, true);
        let done = complete(defrag.submit(&atomic, DOMAIN, 1, 0));
        assert_eq!(done.packet.len(), 56);
        assert_eq!(done.packet[40], 17);
        assert_eq!(&done.packet[48..], b"abcdefgh");
        assert_eq!(defrag.queues.len(), 0);
        assert_eq!(done.first_origin, done.completion_origin);
    }

    #[test]
    fn duplicate_and_partial_overlap_behave_like_linux() {
        let mut defrag = Ipv6Defragmenter::new();
        let first = fragment(3, 0, true, b"abcdefghijklmnop", 0, false);
        let duplicate = fragment(3, 8, true, b"XXXXXXXX", 0, false);
        let overlap = fragment(3, 8, true, b"abcdefghijklmnop", 0, false);
        assert!(matches!(
            defrag.submit(&first, DOMAIN, 1, 0),
            ReassemblyResult::Pending
        ));
        assert!(matches!(
            defrag.submit(&duplicate, DOMAIN, 2, 1),
            ReassemblyResult::Duplicate
        ));
        assert!(matches!(
            defrag.submit(&overlap, DOMAIN, 3, 2),
            ReassemblyResult::Rejected(ReassemblyError::Overlap)
        ));
        assert_eq!(defrag.charged_bytes, 0);
    }

    #[test]
    fn duplicate_final_fragment_records_length_but_does_not_complete() {
        let mut defrag = Ipv6Defragmenter::new();
        let first = fragment(31, 0, true, b"abcdefghijklmnop", 0, false);
        let duplicate_final = fragment(31, 8, false, b"ijklmnop", 0, false);
        assert!(matches!(
            defrag.submit(&first, DOMAIN, 1, 0),
            ReassemblyResult::Pending
        ));
        assert!(matches!(
            defrag.submit(&duplicate_final, DOMAIN, 2, 1),
            ReassemblyResult::Duplicate
        ));
        assert_eq!(defrag.queues[0].final_end, Some(16));
        assert_eq!(defrag.queues[0].received_bytes, 16);
    }

    #[test]
    fn offset_zero_headers_win_even_when_other_fragments_arrive_first() {
        for tail_first in [false, true] {
            let mut defrag = Ipv6Defragmenter::new();
            let first = fragment(4, 0, true, b"abcdefgh", 0, true);
            let mut later = fragment(4, 8, false, b"ijklmnop", 0, true);
            later[47] = 1; // Different unfragmentable Destination Options.
            later[48] = 6; // Different Fragment Next Header.
            later[6] = 0; // Different base Next Header, still a valid chain.
            let (first_arrival, final_arrival) = if tail_first {
                (&later, &first)
            } else {
                (&first, &later)
            };
            assert!(matches!(
                defrag.submit(first_arrival, DOMAIN, 1, 0),
                ReassemblyResult::Pending
            ));
            let done = complete(defrag.submit(final_arrival, DOMAIN, 2, 1));
            assert_eq!(done.packet[6], 60);
            assert_eq!(done.packet[40], 17);
            assert_eq!(done.packet[47], 0);
            assert_eq!(&done.packet[48..], b"abcdefghijklmnop");
            assert_eq!(done.first_origin, if tail_first { 2 } else { 1 });
            assert_eq!(done.completion_origin, 2);
            assert_eq!(defrag.charged_bytes, 0);
        }
    }

    #[test]
    fn rejects_bad_lengths_and_inconsistent_ecn() {
        let mut defrag = Ipv6Defragmenter::new();
        let bad = fragment(5, 0, true, b"123456789", 0, false);
        assert!(matches!(
            defrag.submit(&bad, DOMAIN, 1, 0),
            ReassemblyResult::Rejected(ReassemblyError::Malformed)
        ));
        let first = fragment(5, 0, true, b"abcdefgh", 0, false);
        assert!(matches!(
            defrag.submit(&first, DOMAIN, 1, 1),
            ReassemblyResult::Pending
        ));
        assert!(matches!(
            defrag.submit(&bad, DOMAIN, 1, 2),
            ReassemblyResult::Rejected(ReassemblyError::Malformed)
        ));
        assert_eq!(defrag.queues.len(), 0);
        let last = fragment(5, 8, false, b"ijklmnop", 3, false);
        assert!(matches!(
            defrag.submit(&first, DOMAIN, 1, 3),
            ReassemblyResult::Pending
        ));
        assert!(matches!(
            defrag.submit(&last, DOMAIN, 1, 4),
            ReassemblyResult::Rejected(ReassemblyError::InconsistentEcn)
        ));
        assert_eq!(defrag.queues.len(), 0);
        let first = fragment(6, 0, true, b"abcdefgh", 2, false);
        let last = fragment(6, 8, false, b"ijklmnop", 3, false);
        assert!(matches!(
            defrag.submit(&first, DOMAIN, 1, 3),
            ReassemblyResult::Pending
        ));
        assert_eq!(
            complete(defrag.submit(&last, DOMAIN, 1, 4)).packet[1] & 0x30,
            0x30
        );
    }

    #[test]
    fn domain_and_scoped_interface_isolate_queues() {
        let mut defrag = Ipv6Defragmenter::new();
        let mut first = fragment(7, 0, true, b"abcdefgh", 0, false);
        let mut last = fragment(7, 8, false, b"ijklmnop", 0, false);
        first[24..40].copy_from_slice(&[0xfe, 0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
        last[24..40].copy_from_slice(&first[24..40]);
        assert!(matches!(
            defrag.submit(&first, DOMAIN, 1, 0),
            ReassemblyResult::Pending
        ));
        assert!(matches!(
            defrag.submit(&last, DefragDomain::pre_routing(0, 3), 2, 1),
            ReassemblyResult::Pending
        ));
        assert!(matches!(
            defrag.submit(&last, DefragDomain::local_out(0, 2), 3, 2),
            ReassemblyResult::Pending
        ));
        assert_eq!(complete(defrag.submit(&last, DOMAIN, 4, 3)).first_origin, 1);
        assert_eq!(defrag.queues.len(), 2);
    }

    #[test]
    fn global_unicast_ignores_receive_interface_but_zone_does_not() {
        let mut defrag = Ipv6Defragmenter::new();
        let first = fragment(10, 0, true, b"abcdefgh", 0, false);
        let last = fragment(10, 8, false, b"ijklmnop", 0, false);
        assert!(matches!(
            defrag.submit(&first, DOMAIN, 1, 0),
            ReassemblyResult::Pending
        ));
        assert!(matches!(
            defrag.submit(&last, DefragDomain::pre_routing(1, 3), 2, 1),
            ReassemblyResult::Pending
        ));
        assert_eq!(
            complete(defrag.submit(&last, DefragDomain::pre_routing(0, 3), 4, 2)).first_origin,
            1
        );
        assert_eq!(defrag.queues.len(), 1);
    }

    #[test]
    fn conflicting_final_length_kills_queue() {
        let mut defrag = Ipv6Defragmenter::new();
        let first = fragment(11, 0, true, b"abcdefgh", 0, false);
        let last = fragment(11, 16, false, b"qrstuvwx", 0, false);
        let conflicting = fragment(11, 8, false, b"ijklmnop", 0, false);
        assert!(matches!(
            defrag.submit(&first, DOMAIN, 1, 0),
            ReassemblyResult::Pending
        ));
        assert!(matches!(
            defrag.submit(&last, DOMAIN, 1, 1),
            ReassemblyResult::Pending
        ));
        assert!(matches!(
            defrag.submit(&conflicting, DOMAIN, 1, 2),
            ReassemblyResult::Rejected(ReassemblyError::InconsistentLength)
        ));
        assert_eq!(defrag.charged_bytes, 0);
    }

    #[test]
    fn timeout_and_queue_budget_are_bounded() {
        let mut defrag = Ipv6Defragmenter::new();
        let first = fragment(8, 0, true, b"abcdefgh", 0, false);
        assert!(matches!(
            defrag.submit(&first, DOMAIN, 1, 10),
            ReassemblyResult::Pending
        ));
        assert_eq!(defrag.next_deadline_us(), Some(10 + REASSEMBLY_TIMEOUT_US));
        assert_eq!(defrag.expire(10 + REASSEMBLY_TIMEOUT_US), 1);
        assert_eq!(defrag.charged_bytes, 0);
        for id in 0..MAX_QUEUES + 2 {
            let packet = fragment(100 + id as u32, 0, true, b"abcdefgh", 0, false);
            assert!(matches!(
                defrag.submit(&packet, DOMAIN, 1, 100),
                ReassemblyResult::Pending
            ));
        }
        assert_eq!(defrag.queues.len(), MAX_QUEUES);
        assert!(defrag.charged_bytes <= MAX_STORED_BYTES);
    }

    #[test]
    fn non_fragmented_ipv6_is_not_claimed() {
        let mut defrag = Ipv6Defragmenter::new();
        let mut bytes = fragment(9, 0, false, b"abcdefgh", 0, false);
        assert_eq!(fragment_offset(&bytes, DOMAIN).unwrap(), Some(0));
        bytes[6] = 17;
        assert_eq!(fragment_offset(&bytes, DOMAIN).unwrap(), None);
        assert!(matches!(
            defrag.submit(&bytes, DOMAIN, 1, 0),
            ReassemblyResult::NotFragment
        ));
        bytes[4..6].fill(0);
        assert_eq!(
            fragment_offset(&bytes[..IPV6_HEADER_LEN], DOMAIN).unwrap(),
            None
        );
        assert!(matches!(
            defrag.submit(&bytes[..IPV6_HEADER_LEN], DOMAIN, 1, 1),
            ReassemblyResult::NotFragment
        ));
    }
}
