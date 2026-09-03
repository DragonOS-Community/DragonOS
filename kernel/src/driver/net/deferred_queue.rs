use super::{
    deferred_index::{DeferredRouteIndex, NodeId},
    local_queue::{LocalOutputDisposition, LocalOutputPacket},
};
use alloc::{collections::VecDeque, vec::Vec};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct DeferredRouteKey {
    pub(super) oif: u32,
    pub(super) next_hop: smoltcp::wire::Ipv4Address,
}

impl DeferredRouteKey {
    fn packed(self) -> u64 {
        ((self.oif as u64) << 32) | u32::from_be_bytes(self.next_hop.octets()) as u64
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
    packets: VecDeque<LocalOutputPacket>,
    bytes: usize,
}

pub(super) enum JoinDeferredResult {
    Queued(smoltcp::time::Instant),
    Missing(LocalOutputPacket),
    Full(LocalOutputPacket),
}

#[derive(Clone, Copy)]
pub(super) struct DeferredRouteLimits {
    pub(super) frames: usize,
    pub(super) bytes: usize,
}

/// Per-interface neighbor-resolution backlog.
///
/// The hash index gives direct access by `(oif, next_hop)`, while `heap` is an
/// indexed min-heap whose root is the next schedulable neighbor. In-flight
/// buckets sort after every schedulable bucket and therefore never publish a
/// stale retry deadline. Both structures are protected by the containing
/// local-output lock; this object deliberately owns no lock of its own.
#[derive(Debug, Default)]
pub(super) struct DeferredRouteQueue {
    heap: Vec<DeferredRouteBucket>,
    index: DeferredRouteIndex,
}

impl DeferredRouteQueue {
    pub(super) fn is_empty(&self) -> bool {
        self.heap.is_empty()
    }

    pub(super) fn contains(&self, key: DeferredRouteKey) -> bool {
        self.index.get(key.packed()).is_some()
    }

    pub(super) fn try_enqueue(
        &mut self,
        packet: LocalOutputPacket,
        bytes: usize,
        retry_at: smoltcp::time::Instant,
        probe_sent: bool,
        advance_existing_probe: bool,
        limits: DeferredRouteLimits,
    ) -> Result<smoltcp::time::Instant, LocalOutputPacket> {
        let LocalOutputDisposition::Routed { oif, next_hop, .. } = packet.disposition else {
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
                || bucket.packets.try_reserve(1).is_err()
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
            bucket.packets.push_back(packet);
            let retry_at = bucket.retry_at;
            self.repair(index);
            self.debug_validate();
            return Ok(retry_at);
        }

        let mut packets = VecDeque::new();
        if self.heap.try_reserve(1).is_err()
            || self.index.try_reserve_insert().is_err()
            || packets.try_reserve(1).is_err()
        {
            return Err(packet);
        }
        packets.push_back(packet);
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
        packet: LocalOutputPacket,
        bytes: usize,
        limits: DeferredRouteLimits,
    ) -> JoinDeferredResult {
        let LocalOutputDisposition::Routed { oif, next_hop, .. } = packet.disposition else {
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
            || bucket.packets.try_reserve(1).is_err()
        {
            return JoinDeferredResult::Full(packet);
        }
        let retry_at = bucket.retry_at;
        bucket.bytes += bytes;
        bucket.packets.push_back(packet);
        JoinDeferredResult::Queued(retry_at)
    }

    pub(super) fn pop_due(
        &mut self,
        now: smoltcp::time::Instant,
        max_probes: u8,
        ready: &mut VecDeque<LocalOutputPacket>,
    ) -> Option<(LocalOutputPacket, Option<DeferredRouteKey>)> {
        let bucket = self.heap.first()?;
        if bucket.probe_in_flight || bucket.retry_at > now {
            return None;
        }
        if bucket.resolved || bucket.probes >= max_probes {
            let mut bucket = self.remove_at(0);
            if !bucket.resolved {
                for packet in bucket.packets.iter_mut() {
                    packet.disposition = LocalOutputDisposition::Drop;
                }
            }
            let packet = bucket.packets.pop_front()?;
            ready.append(&mut bucket.packets);
            self.debug_validate();
            return Some((packet, None));
        }

        let bucket = &mut self.heap[0];
        let packet = bucket
            .packets
            .pop_front()
            .expect("a schedulable deferred bucket contains a packet");
        let bytes = packet.frame.capacity();
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
        packet: LocalOutputPacket,
        bytes: usize,
        key: DeferredRouteKey,
        retry_at: smoltcp::time::Instant,
        probe_sent: bool,
        ready: &mut VecDeque<LocalOutputPacket>,
    ) -> Result<bool, LocalOutputPacket> {
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
            bucket.packets.push_front(packet);
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
        ready: &mut VecDeque<LocalOutputPacket>,
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
        ready: &mut VecDeque<LocalOutputPacket>,
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
        ready: &mut VecDeque<LocalOutputPacket>,
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
        mut is_resolved: impl FnMut(smoltcp::wire::Ipv4Address) -> bool,
        ready: &mut VecDeque<LocalOutputPacket>,
    ) {
        let mut removed_any = false;
        let index = &mut self.index;
        self.heap.retain_mut(|bucket| {
            if !is_resolved(bucket.key.next_hop) {
                return true;
            }
            if bucket.probe_in_flight {
                bucket.resolved = true;
                true
            } else {
                ready.append(&mut bucket.packets);
                let removed_slot = index.remove(bucket.key.packed());
                debug_assert!(removed_slot.is_some());
                removed_any = true;
                false
            }
        });
        if removed_any {
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
            .first()
            .is_some_and(|bucket| !bucket.probe_in_flight && bucket.retry_at <= now)
    }

    pub(super) fn next_retry(&self) -> Option<smoltcp::time::Instant> {
        self.heap
            .first()
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
