//! Bounded IPv4 datagram reassembly for a network namespace's early Netfilter path.
//!
//! The caller supplies the Linux defrag user/zone and virtual-interface domain.
//! This object does not route or evaluate rules. A fragment handed to it must
//! not also enter a device's ordinary fragment assembler; only the completed
//! datagram may re-enter the receive path ahead of PRE_ROUTING.

extern crate alloc;

use alloc::vec::Vec;
use core::mem::size_of;

const MAX_DATAGRAM_LEN: usize = u16::MAX as usize;
const MAX_QUEUES: usize = 128;
const MAX_FRAGMENTS_PER_QUEUE: usize = 8192;
const MAX_STORED_BYTES: usize = 4 * 1024 * 1024;
const REASSEMBLY_TIMEOUT_US: u64 = 30_000_000;
// Linux 6.6 include/net/ip.h: IP_DEFRAG_CONNTRACK_IN starts at 2; each CT
// hook reserves one full u16 zone range in the fragment key's user field.
const CT_IN_USER: u32 = 2;
const CT_OUT_USER: u32 = CT_IN_USER + u16::MAX as u32 + 1;
const LOCAL_IN_USER: u32 = CT_OUT_USER + u16::MAX as u32 + 1;

/// The namespace itself is implicit: each namespace owns one reassembler.
/// `user` distinguishes PRE_ROUTING from LOCAL_OUT and carries the CT zone;
/// `vif` is Linux's L3-master interface index (zero without an L3 master).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DefragDomain {
    user: u32,
    vif: u32,
}

impl DefragDomain {
    pub(crate) const fn pre_routing(zone: u16, vif: u32) -> Self {
        Self {
            user: CT_IN_USER + zone as u32,
            vif,
        }
    }

    pub(crate) const fn local_out(zone: u16, vif: u32) -> Self {
        Self {
            user: CT_OUT_USER + zone as u32,
            vif,
        }
    }

    pub(crate) const fn local_input(zone: u16, vif: u32) -> Self {
        Self {
            user: LOCAL_IN_USER + zone as u32,
            vif,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Key {
    source: [u8; 4],
    destination: [u8; 4],
    identification: u16,
    protocol: u8,
    domain: DefragDomain,
}

#[derive(Debug)]
struct Fragment {
    offset: usize,
    payload: Vec<u8>,
}

impl Fragment {
    fn end(&self) -> usize {
        self.offset + self.payload.len()
    }
}

#[derive(Debug)]
struct Queue<M> {
    key: Key,
    expires_at_us: u64,
    fragments: Vec<Fragment>,
    first_header: Option<Vec<u8>>,
    first_origin: Option<M>,
    final_end: Option<usize>,
    largest_end: usize,
    received_bytes: usize,
    ecn_seen: u8,
    largest_fragment: usize,
    largest_df_fragment: usize,
    charged_bytes: usize,
}

/// The first fragment supplies the IP header and its link-layer source;
/// the completing fragment supplies the receive device, as in Linux's
/// `ip_frag_reasm()`. The packet has no remaining fragment offset.
pub(crate) struct ReassembledIpv4<M> {
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
    Pending,
    Duplicate,
    Complete(ReassembledIpv4<M>),
    Rejected(ReassemblyError),
}

/// The caller serializes this small state (for example with the namespace's
/// SpinLock). No rule, FIB, device or socket locks are acquired here.
pub(crate) struct Ipv4Defragmenter<M> {
    queues: Vec<Queue<M>>,
    charged_bytes: usize,
}

struct Parsed<'a> {
    key: Key,
    header: &'a [u8],
    payload: &'a [u8],
    offset: usize,
    more: bool,
    df: bool,
    ecn: u8,
    fragment_len: usize,
}

impl Parsed<'_> {
    fn parse<'a>(bytes: &'a [u8], domain: DefragDomain) -> Result<Parsed<'a>, ReassemblyError> {
        if bytes.len() < 20 || bytes[0] >> 4 != 4 {
            return Err(ReassemblyError::Malformed);
        }
        let header_len = usize::from(bytes[0] & 0x0f) * 4;
        if !(20..=60).contains(&header_len) || header_len > bytes.len() {
            return Err(ReassemblyError::Malformed);
        }
        let total_len = usize::from(u16::from_be_bytes([bytes[2], bytes[3]]));
        if total_len < header_len
            || total_len > bytes.len()
            || !checksum_valid(&bytes[..header_len])
        {
            return Err(ReassemblyError::Malformed);
        }
        let flags_offset = u16::from_be_bytes([bytes[6], bytes[7]]);
        let offset = usize::from(flags_offset & 0x1fff) * 8;
        let more = flags_offset & 0x2000 != 0;
        if !more && offset == 0 {
            return Err(ReassemblyError::Malformed);
        }
        let raw_payload = &bytes[header_len..total_len];
        // Linux ip_frag_queue truncates an unaligned non-final fragment,
        // whereas its offset and all subsequent fragments remain 8-aligned.
        let payload_len = if more {
            raw_payload.len() & !7
        } else {
            raw_payload.len()
        };
        if offset
            .checked_add(payload_len)
            .is_none_or(|end| end > MAX_DATAGRAM_LEN - 20)
        {
            return Err(ReassemblyError::Malformed);
        }
        Ok(Parsed {
            key: Key {
                source: bytes[12..16].try_into().unwrap(),
                destination: bytes[16..20].try_into().unwrap(),
                identification: u16::from_be_bytes([bytes[4], bytes[5]]),
                protocol: bytes[9],
                domain,
            },
            header: &bytes[..header_len],
            payload: &raw_payload[..payload_len],
            offset,
            more,
            df: flags_offset & 0x4000 != 0,
            ecn: bytes[1] & 3,
            fragment_len: header_len + payload_len,
        })
    }
}

impl<M: Clone> Ipv4Defragmenter<M> {
    pub(crate) const fn new() -> Self {
        Self {
            queues: Vec::new(),
            charged_bytes: 0,
        }
    }

    pub(crate) fn next_deadline_us(&self) -> Option<u64> {
        self.queues.iter().map(|queue| queue.expires_at_us).min()
    }

    /// A caller can arm this deadline on the namespace poller. Expiry is also
    /// checked on every insertion so stale entries never pin the byte budget.
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
            Ok(parsed) => parsed,
            Err(error) => return ReassemblyResult::Rejected(error),
        };
        // Linux ip_frag_queue() discards the entire queue when MF truncation
        // leaves an empty fragment. The key must be parsed first so a prior
        // fragment cannot remain queued after this malformed arrival.
        if parsed.payload.is_empty() {
            if let Some(index) = self.queues.iter().position(|queue| queue.key == parsed.key) {
                self.remove(index);
            }
            return ReassemblyResult::Rejected(ReassemblyError::Malformed);
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
                if self.charged_bytes.saturating_add(charge) > MAX_STORED_BYTES {
                    self.evict_until_room(None, charge);
                }
                if self.charged_bytes.saturating_add(charge) > MAX_STORED_BYTES {
                    return ReassemblyResult::Rejected(ReassemblyError::ResourceLimit);
                }
                self.queues.push(Queue {
                    key: parsed.key,
                    expires_at_us: now_us.saturating_add(REASSEMBLY_TIMEOUT_US),
                    fragments: Vec::new(),
                    first_header: None,
                    first_origin: None,
                    final_end: None,
                    largest_end: 0,
                    received_bytes: 0,
                    ecn_seen: 0,
                    largest_fragment: 0,
                    largest_df_fragment: 0,
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
            || (parsed.offset == 0
                && (end > MAX_DATAGRAM_LEN - parsed.header.len()
                    || queue.largest_end > MAX_DATAGRAM_LEN - parsed.header.len()))
            || (queue
                .first_header
                .as_ref()
                .is_some_and(|header| end > MAX_DATAGRAM_LEN - header.len()))
        {
            self.remove(index);
            return ReassemblyResult::Rejected(ReassemblyError::InconsistentLength);
        }

        // Linux's inet_frag_queue_insert ignores a fragment wholly covered by
        // a continuous existing run, even if its bytes differ. Partial overlap
        // kills the entire queue to prevent ambiguous transport payloads.
        let mut covered = parsed.offset;
        let mut overlaps = false;
        let first_overlap = queue
            .fragments
            .partition_point(|fragment| fragment.end() <= parsed.offset);
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

        let mut payload = Vec::new();
        if payload.try_reserve_exact(parsed.payload.len()).is_err() {
            self.remove(index);
            return ReassemblyResult::Rejected(ReassemblyError::OutOfMemory);
        }
        payload.extend_from_slice(parsed.payload);
        let mut header = None;
        if parsed.offset == 0 {
            let mut first = Vec::new();
            if first.try_reserve_exact(parsed.header.len()).is_err() {
                self.remove(index);
                return ReassemblyResult::Rejected(ReassemblyError::OutOfMemory);
            }
            first.extend_from_slice(parsed.header);
            header = Some(first);
        }
        let old_fragment_capacity = self.queues[index].fragments.capacity();
        if self.queues[index].fragments.try_reserve(1).is_err() {
            self.remove(index);
            return ReassemblyResult::Rejected(ReassemblyError::OutOfMemory);
        }
        let fragment_capacity_delta =
            self.queues[index].fragments.capacity() - old_fragment_capacity;
        let charge = payload
            .capacity()
            .saturating_add(header.as_ref().map_or(0, Vec::capacity))
            .saturating_add(fragment_capacity_delta.saturating_mul(size_of::<Fragment>()));
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
        queue.largest_fragment = queue.largest_fragment.max(parsed.fragment_len);
        if parsed.df {
            queue.largest_df_fragment = queue.largest_df_fragment.max(parsed.fragment_len);
        }
        queue.ecn_seen |= 1 << parsed.ecn;
        if let Some(header) = header {
            queue.first_header = Some(header);
            // The completion fragment may also be offset zero. Retain both
            // origins without requiring packet-owned sidecars to be Copy.
            queue.first_origin = Some(origin.clone());
        }
        if !parsed.more {
            queue.final_end = Some(end);
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
        if queue.first_header.is_none() || queue.received_bytes != final_end {
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

fn assemble<M>(
    mut queue: Queue<M>,
    completion_origin: M,
) -> Result<ReassembledIpv4<M>, ReassemblyError> {
    let header = queue
        .first_header
        .take()
        .ok_or(ReassemblyError::Malformed)?;
    let first_origin = queue
        .first_origin
        .take()
        .ok_or(ReassemblyError::Malformed)?;
    let final_end = queue.final_end.ok_or(ReassemblyError::Malformed)?;
    let total_len = header
        .len()
        .checked_add(final_end)
        .ok_or(ReassemblyError::Malformed)?;
    if total_len > MAX_DATAGRAM_LEN {
        return Err(ReassemblyError::InconsistentLength);
    }
    let not_ect = queue.ecn_seen & 1 != 0;
    if not_ect && queue.ecn_seen & !1 != 0 {
        return Err(ReassemblyError::InconsistentEcn);
    }
    let mut packet = Vec::new();
    packet
        .try_reserve_exact(total_len)
        .map_err(|_| ReassemblyError::OutOfMemory)?;
    packet.extend_from_slice(&header);
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
    packet[2..4].copy_from_slice(&(total_len as u16).to_be_bytes());
    if queue.ecn_seen & (1 << 3) != 0 {
        packet[1] = (packet[1] & !3) | 3;
    }
    let df = if queue.largest_df_fragment == queue.largest_fragment {
        0x4000u16
    } else {
        0
    };
    packet[6..8].copy_from_slice(&df.to_be_bytes());
    packet[10..12].fill(0);
    let checksum = !checksum_sum(&packet[..header.len()]);
    packet[10..12].copy_from_slice(&checksum.to_be_bytes());
    Ok(ReassembledIpv4 {
        packet,
        first_origin,
        completion_origin,
    })
}

fn checksum_sum(bytes: &[u8]) -> u16 {
    let mut sum = 0u32;
    for chunk in bytes.chunks_exact(2) {
        sum += u32::from(u16::from_be_bytes([chunk[0], chunk[1]]));
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    sum as u16
}

fn checksum_valid(header: &[u8]) -> bool {
    checksum_sum(header) == u16::MAX
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::{sync::Arc, vec};

    const DOMAIN: DefragDomain = DefragDomain::pre_routing(0, 0);

    fn fragment(
        identification: u16,
        offset: usize,
        more: bool,
        payload: &[u8],
        ecn: u8,
        options: &[u8],
    ) -> Vec<u8> {
        assert!(offset.is_multiple_of(8));
        assert!(options.len().is_multiple_of(4));
        let header_len = 20 + options.len();
        let mut packet = vec![0; header_len + payload.len()];
        let total_len = packet.len() as u16;
        packet[0] = 0x40 | (header_len / 4) as u8;
        packet[1] = ecn;
        packet[2..4].copy_from_slice(&total_len.to_be_bytes());
        packet[4..6].copy_from_slice(&identification.to_be_bytes());
        let flags = (offset / 8) as u16 | if more { 0x2000 } else { 0 };
        packet[6..8].copy_from_slice(&flags.to_be_bytes());
        packet[8] = 64;
        packet[9] = 17;
        packet[12..16].copy_from_slice(&[192, 0, 2, 1]);
        packet[16..20].copy_from_slice(&[192, 0, 2, 2]);
        packet[20..header_len].copy_from_slice(options);
        packet[header_len..].copy_from_slice(payload);
        let checksum = !checksum_sum(&packet[..header_len]);
        packet[10..12].copy_from_slice(&checksum.to_be_bytes());
        packet
    }

    fn completed(result: ReassemblyResult<u8>) -> ReassembledIpv4<u8> {
        let ReassemblyResult::Complete(packet) = result else {
            panic!("expected completed datagram");
        };
        packet
    }

    #[test]
    fn out_of_order_completion_keeps_non_copy_first_fragment_sidecar() {
        #[derive(Clone)]
        struct Origin {
            sidecar: Arc<u32>,
        }

        let first_sidecar = Arc::new(17);
        let last_sidecar = Arc::new(99);
        let first = fragment(42, 0, true, b"abcdefgh", 0, &[]);
        let middle = fragment(42, 8, true, b"ijklmnop", 0, &[]);
        let last = fragment(42, 16, false, b"qrst", 0, &[]);
        let mut assembler = Ipv4Defragmenter::new();
        assert!(matches!(
            assembler.submit(
                &last,
                DOMAIN,
                Origin {
                    sidecar: last_sidecar,
                },
                0,
            ),
            ReassemblyResult::Pending
        ));
        assert!(matches!(
            assembler.submit(
                &first,
                DOMAIN,
                Origin {
                    sidecar: first_sidecar.clone(),
                },
                1,
            ),
            ReassemblyResult::Pending
        ));
        let ReassemblyResult::Complete(datagram) = assembler.submit(
            &middle,
            DOMAIN,
            Origin {
                sidecar: Arc::new(22),
            },
            2,
        ) else {
            panic!("all fragments must complete the datagram")
        };
        assert!(Arc::ptr_eq(&datagram.first_origin.sidecar, &first_sidecar));
        assert_eq!(*datagram.completion_origin.sidecar, 22);
    }

    #[test]
    fn out_of_order_preserves_offset_zero_origin_and_header() {
        let mut assembler = Ipv4Defragmenter::new();
        let last = fragment(1, 16, false, b"qrst", 0, &[]);
        let middle = fragment(1, 8, true, b"ijklmnop", 0, &[]);
        let first = fragment(1, 0, true, b"abcdefgh", 0, &[1, 1, 1, 1]);
        assert!(matches!(
            assembler.submit(&last, DOMAIN, 3, 0),
            ReassemblyResult::Pending
        ));
        assert!(matches!(
            assembler.submit(&middle, DOMAIN, 4, 1),
            ReassemblyResult::Pending
        ));
        let completed = completed(assembler.submit(&first, DOMAIN, 7, 2));
        assert_eq!(completed.first_origin, 7);
        assert_eq!(completed.completion_origin, 7);
        assert_eq!(&completed.packet[24..], b"abcdefghijklmnopqrst");
        assert_eq!(&completed.packet[20..24], &[1, 1, 1, 1]);
        assert_eq!(
            u16::from_be_bytes([completed.packet[2], completed.packet[3]]),
            44
        );
        assert_eq!(&completed.packet[6..8], &[0, 0]);
        assert!(checksum_valid(&completed.packet[..24]));
        assert_eq!(assembler.charged_bytes, 0);
    }

    #[test]
    fn fully_covered_duplicate_is_ignored_but_partial_overlap_kills_queue() {
        let mut assembler = Ipv4Defragmenter::new();
        let first = fragment(2, 0, true, b"abcdefghijklmnop", 0, &[]);
        let duplicate = fragment(2, 8, true, b"XXXXXXXX", 0, &[]);
        let partial_overlap = fragment(2, 8, true, b"abcdefghijklmnop", 0, &[]);
        assert!(matches!(
            assembler.submit(&first, DOMAIN, 1, 0),
            ReassemblyResult::Pending
        ));
        assert!(matches!(
            assembler.submit(&duplicate, DOMAIN, 2, 1),
            ReassemblyResult::Duplicate
        ));
        assert!(matches!(
            assembler.submit(&partial_overlap, DOMAIN, 3, 2),
            ReassemblyResult::Rejected(ReassemblyError::Overlap)
        ));
        assert_eq!(assembler.charged_bytes, 0);
        assert!(matches!(
            assembler.submit(&first, DOMAIN, 1, 3),
            ReassemblyResult::Pending
        ));
    }

    #[test]
    fn duplicate_covering_a_contiguous_existing_run_is_ignored() {
        let mut assembler = Ipv4Defragmenter::new();
        let first = fragment(9, 0, true, b"abcdefgh", 0, &[]);
        let second = fragment(9, 8, true, b"ijklmnop", 0, &[]);
        let duplicate = fragment(9, 0, true, b"XXXXXXXXXXXXXXXX", 0, &[]);
        let last = fragment(9, 16, false, b"qrst", 0, &[]);
        assert!(matches!(
            assembler.submit(&first, DOMAIN, 1, 0),
            ReassemblyResult::Pending
        ));
        assert!(matches!(
            assembler.submit(&second, DOMAIN, 1, 1),
            ReassemblyResult::Pending
        ));
        assert!(matches!(
            assembler.submit(&duplicate, DOMAIN, 1, 2),
            ReassemblyResult::Duplicate
        ));
        let assembled = completed(assembler.submit(&last, DOMAIN, 1, 3));
        assert_eq!(&assembled.packet[20..], b"abcdefghijklmnopqrst");
    }

    #[test]
    fn non_final_fragment_is_trimmed_like_linux() {
        let mut assembler = Ipv4Defragmenter::new();
        let first = fragment(3, 0, true, b"abcdefghX", 0, &[]);
        let last = fragment(3, 8, false, b"ij", 0, &[]);
        assert!(matches!(
            assembler.submit(&first, DOMAIN, 1, 0),
            ReassemblyResult::Pending
        ));
        let completed = completed(assembler.submit(&last, DOMAIN, 1, 1));
        assert_eq!(&completed.packet[20..], b"abcdefghij");
    }

    #[test]
    fn truncated_zero_length_non_final_fragment_kills_existing_queue() {
        let mut assembler = Ipv4Defragmenter::new();
        let first = fragment(10, 0, true, b"abcdefgh", 0, &[]);
        let empty_after_trim = fragment(10, 8, true, b"X", 0, &[]);
        assert!(matches!(
            assembler.submit(&first, DOMAIN, 1, 0),
            ReassemblyResult::Pending
        ));
        assert!(matches!(
            assembler.submit(&empty_after_trim, DOMAIN, 1, 1),
            ReassemblyResult::Rejected(ReassemblyError::Malformed)
        ));
        assert_eq!(assembler.queues.len(), 0);
        assert_eq!(assembler.charged_bytes, 0);
    }

    #[test]
    fn conflicting_final_length_discards_whole_queue() {
        let mut assembler = Ipv4Defragmenter::new();
        let first = fragment(4, 0, true, b"abcdefgh", 0, &[]);
        let last = fragment(4, 16, false, b"qrst", 0, &[]);
        let incompatible = fragment(4, 8, false, b"ijkl", 0, &[]);
        assert!(matches!(
            assembler.submit(&first, DOMAIN, 1, 0),
            ReassemblyResult::Pending
        ));
        assert!(matches!(
            assembler.submit(&last, DOMAIN, 1, 1),
            ReassemblyResult::Pending
        ));
        assert!(matches!(
            assembler.submit(&incompatible, DOMAIN, 1, 2),
            ReassemblyResult::Rejected(ReassemblyError::InconsistentLength)
        ));
        assert_eq!(assembler.charged_bytes, 0);
    }

    #[test]
    fn ecn_ce_propagates_and_not_ect_mixture_is_rejected() {
        let mut assembler = Ipv4Defragmenter::new();
        let ect = fragment(5, 0, true, b"abcdefgh", 2, &[]);
        let ce = fragment(5, 8, false, b"ij", 3, &[]);
        assert!(matches!(
            assembler.submit(&ect, DOMAIN, 1, 0),
            ReassemblyResult::Pending
        ));
        let completed = completed(assembler.submit(&ce, DOMAIN, 1, 1));
        assert_eq!(completed.packet[1] & 3, 3);
        let not_ect = fragment(6, 0, true, b"abcdefgh", 0, &[]);
        let ce = fragment(6, 8, false, b"ij", 3, &[]);
        assert!(matches!(
            assembler.submit(&not_ect, DOMAIN, 1, 2),
            ReassemblyResult::Pending
        ));
        assert!(matches!(
            assembler.submit(&ce, DOMAIN, 1, 3),
            ReassemblyResult::Rejected(ReassemblyError::InconsistentEcn)
        ));
    }

    #[test]
    fn checksum_and_key_domain_isolation() {
        let mut assembler = Ipv4Defragmenter::new();
        let first = fragment(7, 0, true, b"abcdefgh", 0, &[]);
        let last = fragment(7, 8, false, b"ij", 0, &[]);
        let mut bad = first.clone();
        bad[8] ^= 1;
        assert!(matches!(
            assembler.submit(&bad, DOMAIN, 1, 0),
            ReassemblyResult::Rejected(ReassemblyError::Malformed)
        ));
        assert!(matches!(
            assembler.submit(&first, DOMAIN, 1, 0),
            ReassemblyResult::Pending
        ));
        let other = DefragDomain::local_out(0, 0);
        assert!(matches!(
            assembler.submit(&last, other, 2, 1),
            ReassemblyResult::Pending
        ));
        let assembled = completed(assembler.submit(&last, DOMAIN, 9, 2));
        assert_eq!(assembled.first_origin, 1);
        assert_eq!(assembled.completion_origin, 9);
        assert_eq!(
            completed(assembler.submit(&first, other, 2, 3)).first_origin,
            2
        );
    }

    #[test]
    fn timeout_is_fixed_at_first_fragment_and_resource_usage_is_bounded() {
        let mut assembler = Ipv4Defragmenter::new();
        let first = fragment(8, 0, true, b"abcdefgh", 0, &[]);
        let later = fragment(8, 8, true, b"ijklmnop", 0, &[]);
        assert!(matches!(
            assembler.submit(&first, DOMAIN, 1, 10),
            ReassemblyResult::Pending
        ));
        let deadline = 10 + REASSEMBLY_TIMEOUT_US;
        assert_eq!(assembler.next_deadline_us(), Some(deadline));
        assert!(matches!(
            assembler.submit(&later, DOMAIN, 1, 20),
            ReassemblyResult::Pending
        ));
        assert_eq!(assembler.next_deadline_us(), Some(deadline));
        assert_eq!(assembler.expire(deadline), 1);
        assert_eq!(assembler.charged_bytes, 0);
        for id in 0..(MAX_QUEUES + 2) {
            let packet = fragment(id as u16, 0, true, b"abcdefgh", 0, &[]);
            assert!(matches!(
                assembler.submit(&packet, DOMAIN, 1, deadline + 1),
                ReassemblyResult::Pending
            ));
        }
        assert_eq!(assembler.queues.len(), MAX_QUEUES);
        assert!(assembler.charged_bytes <= MAX_STORED_BYTES);
    }

    #[test]
    fn global_byte_pressure_evicts_oldest_incomplete_datagrams() {
        let mut assembler = Ipv4Defragmenter::new();
        let payload = vec![0x41; 60_000];
        for id in 0..70 {
            let packet = fragment(id, 0, true, &payload, 0, &[]);
            assert!(matches!(
                assembler.submit(&packet, DOMAIN, 1, u64::from(id)),
                ReassemblyResult::Pending
            ));
            assert!(assembler.charged_bytes <= MAX_STORED_BYTES);
        }
        assert!(assembler.queues.len() < 70);
        assert!(assembler
            .queues
            .iter()
            .all(|queue| queue.key.identification != 0));
        assert!(assembler
            .queues
            .iter()
            .any(|queue| queue.key.identification == 69));
    }
}
