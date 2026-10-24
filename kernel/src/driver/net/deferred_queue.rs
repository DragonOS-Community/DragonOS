use super::{
    deferred_index::{DeferredRouteIndex, NodeId, RouteIndexKey},
    local_queue::{LocalOutputDisposition, OutputNode, OutputQueue, MAX_LOCAL_OUTPUT_FRAMES},
};
use crate::libs::paged_vec::PagedVec;
use system_error::SystemError;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct DeferredRouteKey {
    pub(super) oif: u32,
    pub(super) next_hop: smoltcp::wire::IpAddress,
}

#[cfg(test)]
mod queue_tests {
    use super::super::local_queue::{LocalOutputPacket, OutputCtContext, OutputEnvelope};
    use super::*;
    use crate::libs::prepared_queue::Node;

    fn key(last: u8) -> DeferredRouteKey {
        DeferredRouteKey {
            oif: 7,
            next_hop: smoltcp::wire::IpAddress::Ipv4(smoltcp::wire::Ipv4Address::new(
                192, 0, 2, last,
            )),
        }
    }

    fn packet(key: DeferredRouteKey, mark: u32) -> OutputNode {
        Node::try_new(OutputEnvelope {
            packet: Some(LocalOutputPacket {
                medium: smoltcp::phy::Medium::Ip,
                meta: smoltcp::phy::PacketMeta::default(),
                disposition: LocalOutputDisposition::Routed {
                    oif: key.oif,
                    next_hop: key.next_hop,
                    ip_mtu: 1500,
                },
                frame: Default::default(),
                ct_context: OutputCtContext::Untracked,
                mark,
                prepared_ip: None,
                forward_mtu_feedback: None,
                _charge: None,
            }),
            retry_at: smoltcp::time::Instant::from_millis(0),
        })
        .unwrap()
    }

    fn limits() -> DeferredRouteLimits {
        DeferredRouteLimits {
            frames: 8,
            bytes: 4096,
        }
    }

    #[test]
    fn probe_and_neighbor_release_transfer_the_same_nodes_in_fifo_order() {
        let mut deferred = DeferredRouteQueue::default();
        let mut ready = OutputQueue::new();
        let now = smoltcp::time::Instant::from_millis(1);
        let first = packet(key(1), 1);
        let identity = first.as_ref() as *const _;
        deferred.prepare_capacity(2).unwrap();
        assert!(deferred
            .try_enqueue(first, 0, now, false, false, limits())
            .is_ok());
        assert!(deferred
            .try_enqueue(packet(key(1), 2), 0, now, false, false, limits())
            .is_ok());
        let (probe, probe_key) = deferred.pop_due(now, 3, &mut ready).unwrap();
        assert_eq!(probe.as_ref() as *const _, identity);
        assert_eq!(probe_key, Some(key(1)));
        assert!(deferred.release_neighbor(key(1), &mut ready));
        assert!(deferred
            .finish_probe(probe, 0, key(1), now, false, &mut ready)
            .unwrap());
        assert!(deferred.is_empty());
        assert_eq!(ready.pop_node().unwrap().as_ref() as *const _, identity);
        assert_eq!(ready.pop_node().unwrap().value().mark, 2);
        assert!(ready.is_empty());
    }

    #[test]
    fn unprepared_admission_returns_its_owner_without_publishing() {
        let mut deferred = DeferredRouteQueue::default();
        let now = smoltcp::time::Instant::from_millis(0);
        let node = packet(key(1), 1);
        let identity = node.as_ref() as *const _;
        let returned = deferred
            .try_enqueue(node, 0, now, false, false, limits())
            .unwrap_err();
        assert_eq!(returned.as_ref() as *const _, identity);
        assert!(deferred.is_empty());
        assert!(!deferred.contains(key(1)));
    }

    #[test]
    fn bulk_release_preserves_index_and_heap_after_compaction() {
        let mut deferred = DeferredRouteQueue::default();
        let mut ready = OutputQueue::new();
        let now = smoltcp::time::Instant::from_millis(1);
        deferred.prepare_capacity(20).unwrap();
        for last in (1..=20).rev() {
            assert!(deferred
                .try_enqueue(
                    packet(key(last), last as u32),
                    0,
                    now,
                    false,
                    false,
                    limits()
                )
                .is_ok());
        }
        deferred.release_resolved(
            |address| match address {
                smoltcp::wire::IpAddress::Ipv4(address) => address.octets()[3] % 2 == 0,
                _ => false,
            },
            &mut ready,
        );
        assert_eq!(ready.len(), 10);
        for last in 1..=20 {
            assert_eq!(deferred.contains(key(last)), last % 2 != 0);
        }
        let mut previous = 0;
        while let Some((node, probe_key)) = deferred.pop_due(now, 0, &mut ready) {
            assert!(probe_key.is_none());
            assert!(node.value().mark > previous);
            previous = node.value().mark;
            assert!(matches!(
                node.value().disposition,
                LocalOutputDisposition::Drop
            ));
        }
        assert_eq!(previous, 19);
        assert!(deferred.is_empty());
    }
}

impl DeferredRouteKey {
    fn packed(self) -> RouteIndexKey {
        let mut key = [0; 21];
        key[1..5].copy_from_slice(&self.oif.to_be_bytes());
        match self.next_hop {
            smoltcp::wire::IpAddress::Ipv4(address) => {
                key[0] = 4;
                key[17..].copy_from_slice(&address.octets());
            }
            smoltcp::wire::IpAddress::Ipv6(address) => {
                key[0] = 6;
                key[5..].copy_from_slice(&address.octets());
            }
        }
        key
    }
}

#[cfg(test)]
mod key_tests {
    use super::*;
    use smoltcp::wire::{IpAddress, Ipv4Address, Ipv6Address};

    #[test]
    fn family_device_and_all_address_bytes_identify_a_neighbor() {
        let v4 = DeferredRouteKey {
            oif: 2,
            next_hop: IpAddress::Ipv4(Ipv4Address::new(192, 0, 2, 1)),
        };
        let mut bytes = [0; 16];
        bytes[12..].copy_from_slice(&[192, 0, 2, 1]);
        let v6 = DeferredRouteKey {
            oif: 2,
            next_hop: IpAddress::Ipv6(Ipv6Address::from(bytes)),
        };
        assert_ne!(v4.packed(), v6.packed());
        assert_ne!(v6.packed(), DeferredRouteKey { oif: 3, ..v6 }.packed());
        for byte in 0..16 {
            let mut different = bytes;
            different[byte] ^= 0x80;
            assert_ne!(
                v6.packed(),
                DeferredRouteKey {
                    next_hop: IpAddress::Ipv6(Ipv6Address::from(different)),
                    ..v6
                }
                .packed()
            );
        }
    }
}

#[derive(Debug)]
struct DeferredRouteBucket {
    key: DeferredRouteKey,
    index_leaf: NodeId,
    retry_at: smoltcp::time::Instant,
    probes: u8,
    probe_in_flight: bool,
    probe_bytes: usize,
    resolved: bool,
    packets: OutputQueue,
    bytes: usize,
}

const _: () = assert!(PagedVec::<DeferredRouteBucket, MAX_LOCAL_OUTPUT_FRAMES>::layout_supported());

pub(super) enum JoinDeferredResult {
    Queued(smoltcp::time::Instant),
    Missing(OutputNode),
    Full(OutputNode),
}

#[derive(Clone, Copy)]
pub(super) struct DeferredRouteLimits {
    pub(super) frames: usize,
    pub(super) bytes: usize,
}

/// Per-interface neighbor-resolution backlog.
///
/// The Patricia index gives bounded access by `(oif, next_hop)`, while `heap` is an
/// indexed min-heap whose root is the next schedulable neighbor. In-flight
/// buckets sort after every schedulable bucket and therefore never publish a
/// stale retry deadline. Both structures are protected by the containing
/// local-output lock; this object deliberately owns no lock of its own.
#[derive(Debug, Default)]
pub(super) struct DeferredRouteQueue {
    heap: PagedVec<DeferredRouteBucket, MAX_LOCAL_OUTPUT_FRAMES>,
    index: DeferredRouteIndex,
}

impl DeferredRouteQueue {
    /// Prepare worst-case per-frame neighbor metadata before emission. This
    /// uses the containing queue's total frames plus outstanding reservations,
    /// since the eventual neighbor key is not known until packet serialization.
    pub(super) fn prepare_capacity(&mut self, frames: usize) -> Result<(), SystemError> {
        if frames > MAX_LOCAL_OUTPUT_FRAMES {
            return Err(SystemError::ENOBUFS);
        }
        self.heap
            .try_reserve(frames.saturating_sub(self.heap.len()))?;
        self.index.prepare_capacity(frames)
    }

    pub(super) fn is_empty(&self) -> bool {
        self.heap.is_empty()
    }

    pub(super) fn contains(&self, key: DeferredRouteKey) -> bool {
        self.index.get(key.packed()).is_some()
    }

    pub(super) fn try_enqueue(
        &mut self,
        packet: OutputNode,
        bytes: usize,
        retry_at: smoltcp::time::Instant,
        probe_sent: bool,
        advance_existing_probe: bool,
        limits: DeferredRouteLimits,
    ) -> Result<smoltcp::time::Instant, OutputNode> {
        let LocalOutputDisposition::Routed { oif, next_hop, .. } = packet.value().disposition
        else {
            return Err(packet);
        };
        let key = DeferredRouteKey { oif, next_hop };
        if let Some((_, index)) = self.index.get(key.packed()) {
            let bucket = &mut self.heap[index];
            if bucket.packets.len() + usize::from(bucket.probe_in_flight) >= limits.frames
                || bucket
                    .bytes
                    .saturating_add(bucket.probe_bytes)
                    .saturating_add(bytes)
                    > limits.bytes
            {
                return Err(packet);
            }
            bucket.retry_at = if probe_sent || advance_existing_probe {
                retry_at
            } else {
                bucket.retry_at.min(retry_at)
            };
            if probe_sent {
                bucket.probes = bucket.probes.saturating_add(1);
            }
            bucket.bytes += bytes;
            bucket.packets.push_node(packet);
            let retry_at = bucket.retry_at;
            self.repair(index);
            self.debug_validate();
            return Ok(retry_at);
        }

        let mut packets = OutputQueue::new();
        if limits.frames == 0
            || bytes > limits.bytes
            || self.heap.len() == self.heap.capacity()
            || !self.index.has_insert_capacity()
        {
            return Err(packet);
        }
        packets.push_node(packet);
        let index = self.heap.len();
        let index_leaf = self.index.insert_prepared(key.packed(), index);
        self.heap.push(DeferredRouteBucket {
            key,
            index_leaf,
            retry_at,
            probes: u8::from(probe_sent),
            probe_in_flight: false,
            probe_bytes: 0,
            resolved: false,
            packets,
            bytes,
        });
        self.sift_up(index);
        self.debug_validate();
        Ok(retry_at)
    }

    pub(super) fn try_join(
        &mut self,
        packet: OutputNode,
        bytes: usize,
        limits: DeferredRouteLimits,
    ) -> JoinDeferredResult {
        let LocalOutputDisposition::Routed { oif, next_hop, .. } = packet.value().disposition
        else {
            return JoinDeferredResult::Missing(packet);
        };
        let key = DeferredRouteKey { oif, next_hop };
        let Some((_, index)) = self.index.get(key.packed()) else {
            return JoinDeferredResult::Missing(packet);
        };
        let bucket = &mut self.heap[index];
        if bucket.packets.len() + usize::from(bucket.probe_in_flight) >= limits.frames
            || bucket
                .bytes
                .saturating_add(bucket.probe_bytes)
                .saturating_add(bytes)
                > limits.bytes
        {
            return JoinDeferredResult::Full(packet);
        }
        let retry_at = bucket.retry_at;
        bucket.bytes += bytes;
        bucket.packets.push_node(packet);
        JoinDeferredResult::Queued(retry_at)
    }

    pub(super) fn pop_due(
        &mut self,
        now: smoltcp::time::Instant,
        max_probes: u8,
        ready: &mut OutputQueue,
    ) -> Option<(OutputNode, Option<DeferredRouteKey>)> {
        let bucket = self.heap.iter().next()?;
        if bucket.probe_in_flight || bucket.retry_at > now {
            return None;
        }
        if bucket.resolved || bucket.probes >= max_probes {
            let mut bucket = self.remove_at(0);
            if !bucket.resolved {
                for packet in bucket.packets.iter_mut() {
                    packet.packet_mut().disposition = LocalOutputDisposition::Drop;
                }
            }
            let packet = bucket.packets.pop_node()?;
            ready.append(&mut bucket.packets);
            self.debug_validate();
            return Some((packet, None));
        }

        let bucket = &mut self.heap[0];
        let packet = bucket
            .packets
            .pop_node()
            .expect("a schedulable deferred bucket contains a packet");
        let bytes = packet.value().frame.capacity();
        bucket.bytes -= bytes;
        bucket.probe_in_flight = true;
        bucket.probe_bytes = bytes;
        let key = bucket.key;
        self.repair(0);
        self.debug_validate();
        Some((packet, Some(key)))
    }

    pub(super) fn finish_probe(
        &mut self,
        packet: OutputNode,
        bytes: usize,
        key: DeferredRouteKey,
        retry_at: smoltcp::time::Instant,
        probe_sent: bool,
        ready: &mut OutputQueue,
    ) -> Result<bool, OutputNode> {
        let Some((_, index)) = self.index.get(key.packed()) else {
            return Err(packet);
        };
        if !self.heap[index].probe_in_flight {
            return Err(packet);
        }
        let resolved = self.heap[index].resolved;
        {
            let bucket = &mut self.heap[index];
            debug_assert_eq!(bucket.probe_bytes, bytes);
            bucket.probe_in_flight = false;
            bucket.probe_bytes = 0;
            bucket.bytes += bytes;
            let mut rest = core::mem::take(&mut bucket.packets);
            bucket.packets.push_node(packet);
            bucket.packets.append(&mut rest);
            if !resolved {
                bucket.retry_at = retry_at;
                if probe_sent {
                    bucket.probes = bucket.probes.saturating_add(1);
                }
            }
        }
        if resolved {
            let mut bucket = self.remove_at(index);
            ready.append(&mut bucket.packets);
        } else {
            self.repair(index);
        }
        self.debug_validate();
        Ok(resolved)
    }

    pub(super) fn complete_probe_success(
        &mut self,
        key: DeferredRouteKey,
        ready: &mut OutputQueue,
    ) {
        let Some((_, index)) = self.index.get(key.packed()) else {
            return;
        };
        if !self.heap[index].probe_in_flight {
            return;
        }
        let mut bucket = self.remove_at(index);
        ready.append(&mut bucket.packets);
        self.debug_validate();
    }

    pub(super) fn complete_packet_failure(
        &mut self,
        key: DeferredRouteKey,
        ready: &mut OutputQueue,
    ) {
        let Some((_, index)) = self.index.get(key.packed()) else {
            return;
        };
        if !self.heap[index].probe_in_flight {
            return;
        }
        self.heap[index].probe_in_flight = false;
        self.heap[index].probe_bytes = 0;
        if self.heap[index].packets.is_empty() {
            self.remove_at(index);
        } else if self.heap[index].resolved {
            let mut bucket = self.remove_at(index);
            ready.append(&mut bucket.packets);
        } else {
            self.repair(index);
        }
        self.debug_validate();
    }

    pub(super) fn release_neighbor(
        &mut self,
        key: DeferredRouteKey,
        ready: &mut OutputQueue,
    ) -> bool {
        let Some((_, index)) = self.index.get(key.packed()) else {
            return false;
        };
        if self.heap[index].probe_in_flight {
            self.heap[index].resolved = true;
        } else {
            let mut bucket = self.remove_at(index);
            ready.append(&mut bucket.packets);
        }
        self.debug_validate();
        true
    }

    /// Bulk synchronization with smoltcp's neighbor cache remains O(B), but
    /// compaction and heap rebuilding are each performed only once.
    pub(super) fn release_resolved(
        &mut self,
        mut is_resolved: impl FnMut(smoltcp::wire::IpAddress) -> bool,
        ready: &mut OutputQueue,
    ) {
        let original_len = self.heap.len();
        let mut retained = 0;
        for current in 0..original_len {
            let bucket = &mut self.heap[current];
            if is_resolved(bucket.key.next_hop) {
                if bucket.probe_in_flight {
                    bucket.resolved = true;
                } else {
                    ready.append(&mut bucket.packets);
                    let removed_slot = self.index.remove(bucket.key.packed());
                    debug_assert!(removed_slot.is_some());
                    continue;
                }
            }
            // Stable in-place compaction: removed, now-empty buckets move to
            // the tail without allocating or changing any retained node IDs.
            self.heap.swap(retained, current);
            retained += 1;
        }
        if retained != original_len {
            while self.heap.len() > retained {
                drop(self.heap.pop());
            }
            for (slot, bucket) in self.heap.iter().enumerate() {
                self.index
                    .set_slot(bucket.index_leaf, bucket.key.packed(), slot);
            }
            for slot in (0..self.heap.len() / 2).rev() {
                self.sift_down(slot);
            }
        }
        self.debug_validate();
    }

    pub(super) fn has_due(&self, now: smoltcp::time::Instant) -> bool {
        self.heap
            .iter()
            .next()
            .is_some_and(|bucket| !bucket.probe_in_flight && bucket.retry_at <= now)
    }

    pub(super) fn next_retry(&self) -> Option<smoltcp::time::Instant> {
        self.heap
            .iter()
            .next()
            .filter(|bucket| !bucket.probe_in_flight)
            .map(|bucket| bucket.retry_at)
    }

    fn higher_priority(left: &DeferredRouteBucket, right: &DeferredRouteBucket) -> bool {
        match (left.probe_in_flight, right.probe_in_flight) {
            (false, true) => true,
            (true, false) => false,
            _ => (left.retry_at, left.key.packed()) < (right.retry_at, right.key.packed()),
        }
    }

    fn swap_slots(&mut self, left: usize, right: usize) {
        if left == right {
            return;
        }
        self.heap.swap(left, right);
        let left_bucket = &self.heap[left];
        self.index
            .set_slot(left_bucket.index_leaf, left_bucket.key.packed(), left);
        let right_bucket = &self.heap[right];
        self.index
            .set_slot(right_bucket.index_leaf, right_bucket.key.packed(), right);
    }

    fn sift_up(&mut self, mut index: usize) -> usize {
        while index > 0 {
            let parent = (index - 1) / 2;
            if !Self::higher_priority(&self.heap[index], &self.heap[parent]) {
                break;
            }
            self.swap_slots(index, parent);
            index = parent;
        }
        index
    }

    fn sift_down(&mut self, mut index: usize) {
        loop {
            let left = index * 2 + 1;
            if left >= self.heap.len() {
                break;
            }
            let right = left + 1;
            let child = if right < self.heap.len()
                && Self::higher_priority(&self.heap[right], &self.heap[left])
            {
                right
            } else {
                left
            };
            if !Self::higher_priority(&self.heap[child], &self.heap[index]) {
                break;
            }
            self.swap_slots(index, child);
            index = child;
        }
    }

    fn repair(&mut self, index: usize) {
        let index = self.sift_up(index);
        self.sift_down(index);
    }

    fn remove_at(&mut self, index: usize) -> DeferredRouteBucket {
        let last = self.heap.len() - 1;
        self.swap_slots(index, last);
        let removed = self.heap.pop().expect("remove index is in bounds");
        let removed_slot = self.index.remove(removed.key.packed());
        debug_assert_eq!(removed_slot, Some(last));
        if index < self.heap.len() {
            self.repair(index);
        }
        removed
    }

    fn debug_validate(&self) {
        debug_assert_eq!(self.heap.len(), self.index.len());
        for (index, bucket) in self.heap.iter().enumerate() {
            debug_assert_eq!(
                self.index.get(bucket.key.packed()),
                Some((bucket.index_leaf, index))
            );
            debug_assert!(!bucket.packets.is_empty() || bucket.probe_in_flight);
            if index > 0 {
                debug_assert!(!Self::higher_priority(bucket, &self.heap[(index - 1) / 2]));
            }
        }
    }
}
