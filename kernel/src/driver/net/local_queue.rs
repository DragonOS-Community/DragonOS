pub(super) use super::deferred_queue::DeferredRouteKey;
use super::deferred_queue::{DeferredRouteLimits, DeferredRouteQueue, JoinDeferredResult};
use super::*;

pub(super) struct LocalInputRxToken {
    pub(super) frame: Vec<u8>,
    pub(super) meta: PacketMeta,
}

impl RxToken for LocalInputRxToken {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        f(&self.frame)
    }

    fn meta(&self) -> PacketMeta {
        self.meta
    }
}

#[derive(Debug)]
pub(super) struct LocalInputPacket {
    pub(super) ingress_ifindex: u32,
    pub(super) destination_mac: smoltcp::wire::EthernetAddress,
    pub(super) source_mac: smoltcp::wire::EthernetAddress,
    pub(super) ip_packet: Vec<u8>,
}

impl LocalInputPacket {
    pub(super) fn len(&self) -> usize {
        self.ip_packet.len()
    }

    pub(super) fn into_frame(self, medium: smoltcp::phy::Medium) -> Result<Vec<u8>, SystemError> {
        let ethertype = match smoltcp::wire::IpVersion::of_packet(&self.ip_packet)
            .map_err(|_| SystemError::EINVAL)?
        {
            smoltcp::wire::IpVersion::Ipv4 => [0x08, 0x00],
            smoltcp::wire::IpVersion::Ipv6 => [0x86, 0xdd],
        };
        if medium == smoltcp::phy::Medium::Ip {
            return Ok(self.ip_packet);
        }
        let frame_len = 14usize
            .checked_add(self.ip_packet.len())
            .ok_or(SystemError::EMSGSIZE)?;
        let mut frame = Vec::new();
        frame
            .try_reserve_exact(frame_len)
            .map_err(|_| SystemError::ENOMEM)?;
        frame.extend_from_slice(&self.destination_mac.0);
        frame.extend_from_slice(&self.source_mac.0);
        frame.extend_from_slice(&ethertype);
        frame.extend_from_slice(&self.ip_packet);
        Ok(frame)
    }
}

#[derive(Debug)]
pub(super) struct LocalInputQueueState {
    pub(super) packets: VecDeque<LocalInputPacket>,
    pub(super) bytes: usize,
}

#[derive(Debug)]
pub(super) struct LocalOutputPacket {
    pub(super) medium: smoltcp::phy::Medium,
    pub(super) meta: PacketMeta,
    pub(super) disposition: LocalOutputDisposition,
    pub(super) frame: Vec<u8>,
}

#[derive(Debug)]
pub(super) struct BackpressuredLocalOutput {
    pub(super) retry_at: smoltcp::time::Instant,
    pub(super) packet: LocalOutputPacket,
}

/// Immutable output policy chosen before entering the smoltcp serialization
/// locks. A queued packet is never reclassified against a later FIB snapshot.
#[derive(Debug, Clone, Copy)]
pub(super) enum LocalOutputDisposition {
    NativeOwner,
    Local {
        oif: u32,
        ip_mtu: usize,
    },
    Routed {
        oif: u32,
        next_hop: smoltcp::wire::IpAddress,
        ip_mtu: usize,
    },
    Drop,
}

impl LocalOutputDisposition {
    pub(super) const DROP_CONTEXT: [u64; 3] = [0; 3];
    pub(super) const NATIVE_CONTEXT: [u64; 3] = [1, 0, 0];
    const LOCAL_TAG: u64 = 2;
    const IPV4_TAG: u64 = 4;
    const IPV6_TAG: u64 = 6;

    pub(super) fn routed_context(oif: u32, next_hop: smoltcp::wire::IpAddress) -> [u64; 3] {
        debug_assert_ne!(oif, 0);
        match next_hop {
            smoltcp::wire::IpAddress::Ipv4(address) => [
                ((oif as u64) << 32) | Self::IPV4_TAG,
                0,
                u32::from_be_bytes(address.octets()) as u64,
            ],
            smoltcp::wire::IpAddress::Ipv6(address) => {
                let bytes = address.octets();
                [
                    ((oif as u64) << 32) | Self::IPV6_TAG,
                    u64::from_be_bytes(bytes[..8].try_into().unwrap()),
                    u64::from_be_bytes(bytes[8..].try_into().unwrap()),
                ]
            }
        }
    }

    pub(super) fn local_context(oif: u32) -> [u64; 3] {
        debug_assert_ne!(oif, 0);
        [((oif as u64) << 32) | Self::LOCAL_TAG, 0, 0]
    }

    pub(super) fn from_context(context: [u64; 3], ip_mtu: usize) -> Self {
        if context == Self::NATIVE_CONTEXT {
            return Self::NativeOwner;
        }
        let oif = (context[0] >> 32) as u32;
        if oif == 0 {
            return Self::Drop;
        }
        let next_hop = match context[0] & u32::MAX as u64 {
            Self::LOCAL_TAG if context[1..] == [0, 0] => return Self::Local { oif, ip_mtu },
            Self::IPV4_TAG if context[1] == 0 && context[2] <= u32::MAX as u64 => {
                smoltcp::wire::IpAddress::Ipv4(smoltcp::wire::Ipv4Address::from(
                    (context[2] as u32).to_be_bytes(),
                ))
            }
            Self::IPV6_TAG => {
                let mut bytes = [0; 16];
                bytes[..8].copy_from_slice(&context[1].to_be_bytes());
                bytes[8..].copy_from_slice(&context[2].to_be_bytes());
                smoltcp::wire::IpAddress::Ipv6(smoltcp::wire::Ipv6Address::from(bytes))
            }
            _ => return Self::Drop,
        };
        Self::Routed {
            oif,
            next_hop,
            ip_mtu,
        }
    }
}

#[cfg(test)]
mod context_tests {
    use super::*;
    use smoltcp::wire::{IpAddress, Ipv4Address, Ipv6Address};

    #[test]
    fn routed_context_preserves_both_families_and_full_ipv6_address() {
        let addresses = [
            IpAddress::Ipv4(Ipv4Address::new(192, 0, 2, 1)),
            IpAddress::Ipv6(Ipv6Address::from([
                0xfd, 0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc, 0xde, 0xf0, 0x11, 0x22, 0x33, 0xc0, 0, 2,
                1,
            ])),
        ];
        for address in addresses {
            for ifindex in [1, 42, i32::MAX as u32] {
                let context = LocalOutputDisposition::routed_context(ifindex, address);
                let LocalOutputDisposition::Routed {
                    oif,
                    next_hop,
                    ip_mtu,
                } = LocalOutputDisposition::from_context(context, 1280)
                else {
                    panic!("routed context changed disposition")
                };
                assert_eq!((oif, next_hop, ip_mtu), (ifindex, address, 1280));
            }
        }
        assert!(matches!(
            LocalOutputDisposition::from_context(LocalOutputDisposition::local_context(42), 1500),
            LocalOutputDisposition::Local {
                oif: 42,
                ip_mtu: 1500
            }
        ));
        assert!(matches!(
            LocalOutputDisposition::from_context(LocalOutputDisposition::NATIVE_CONTEXT, 1500),
            LocalOutputDisposition::NativeOwner
        ));
        assert!(matches!(
            LocalOutputDisposition::from_context(LocalOutputDisposition::DROP_CONTEXT, 1500),
            LocalOutputDisposition::Drop
        ));
        // Unknown tags and malformed IPv4 payloads fail closed.
        for context in [[(42 << 32) | 3, 0, 0], [(42 << 32) | 4, 1, 0], [6, 1, 2]] {
            assert!(matches!(
                LocalOutputDisposition::from_context(context, 1500),
                LocalOutputDisposition::Drop
            ));
        }
    }
}

#[derive(Debug, Default)]
pub(super) struct LocalOutputQueueState {
    pub(super) packets: VecDeque<LocalOutputPacket>,
    pub(super) backpressured: VecDeque<BackpressuredLocalOutput>,
    pub(super) deferred_routes: DeferredRouteQueue,
    pub(super) frames: usize,
    pub(super) bytes: usize,
    pub(super) reserved_frames: usize,
    pub(super) reserved_bytes: usize,
}

#[derive(Debug, Default)]
pub(super) struct LocalOutputScratchPool {
    pub(super) buffers: Vec<Vec<u8>>,
    pub(super) bytes: usize,
}

#[derive(Debug)]
pub(super) struct LocalInputQueue {
    pub(super) state: SpinLock<LocalInputQueueState>,
    pub(super) response_scratch: SpinLock<LocalOutputScratchPool>,
    pub(super) output: SpinLock<LocalOutputQueueState>,
    pub(super) output_draining: AtomicBool,
}

pub(super) struct LocalOutputDrainGuard<'a> {
    pub(super) draining: &'a AtomicBool,
    pub(super) active: bool,
}

pub(super) struct LocalOutputReservation<'a> {
    pub(super) output: &'a SpinLock<LocalOutputQueueState>,
    pub(super) bytes: usize,
    pub(super) active: bool,
}

pub(super) enum LocalOutputPop<'a> {
    Ready(
        LocalOutputPacket,
        LocalOutputReservation<'a>,
        Option<DeferredRouteKey>,
    ),
    DeferredUntil(smoltcp::time::Instant),
    Empty,
}

pub(super) enum ExistingDeferredCommit<'a> {
    Queued(smoltcp::time::Instant),
    Missing(LocalOutputPacket, LocalOutputReservation<'a>),
    Full(LocalOutputPacket, LocalOutputReservation<'a>),
}

pub(super) enum ExistingDeferredEnqueue<'a> {
    Queued(smoltcp::time::Instant),
    Missing(LocalOutputPacket, LocalOutputReservation<'a>),
    Full(LocalOutputPacket),
}

pub(super) enum AdmittedRoutedOutput {
    Sent(LocalOutputPacket),
    Queued(smoltcp::time::Instant),
    Drop(LocalOutputPacket, SystemError),
}

impl Drop for LocalOutputDrainGuard<'_> {
    fn drop(&mut self) {
        if self.active {
            self.draining.store(false, Ordering::Release);
        }
    }
}

impl LocalOutputDrainGuard<'_> {
    /// Release drain ownership while serializing the final empty observation
    /// with output producers. A producer is therefore either observed here or
    /// acquires drain ownership after this handoff.
    pub(super) fn finish_and_has_output(
        mut self,
        output: &SpinLock<LocalOutputQueueState>,
    ) -> bool {
        let output = output.lock();
        self.draining.store(false, Ordering::Release);
        self.active = false;
        !output.packets.is_empty()
            || !output.backpressured.is_empty()
            || !output.deferred_routes.is_empty()
    }
}

impl<'a> LocalOutputReservation<'a> {
    /// Move this token's byte reservation without changing the queue-wide
    /// frame reservation. This is used when routing selects a larger MTU.
    pub(super) fn try_resize(&mut self, bytes: usize) -> bool {
        let mut output = self.output.lock();
        let unreserved = output.reserved_bytes - self.bytes;
        if output
            .bytes
            .saturating_add(unreserved)
            .saturating_add(bytes)
            > LocalInputQueue::MAX_BYTES
        {
            return false;
        }
        output.reserved_bytes = unreserved + bytes;
        self.bytes = bytes;
        true
    }

    pub(super) fn commit(
        self,
        medium: smoltcp::phy::Medium,
        meta: PacketMeta,
        disposition: LocalOutputDisposition,
        scratch: &mut LocalInputScratch<'_>,
    ) {
        let frame = scratch
            .take()
            .expect("an admitted local output token owns its scratch buffer");
        debug_assert_eq!(frame.capacity(), self.bytes);
        self.commit_packet(LocalOutputPacket {
            medium,
            meta,
            disposition,
            frame,
        });
    }

    pub(super) fn requeue_backpressured(
        mut self,
        packet: LocalOutputPacket,
        retry_at: smoltcp::time::Instant,
    ) {
        debug_assert_eq!(packet.frame.capacity(), self.bytes);
        let mut output = self.output.lock();
        output.reserved_frames -= 1;
        output.reserved_bytes -= self.bytes;
        output.frames += 1;
        output.bytes += self.bytes;
        let queued = BackpressuredLocalOutput { retry_at, packet };
        if output
            .backpressured
            .back()
            .is_none_or(|tail| tail.retry_at <= retry_at)
        {
            output.backpressured.push_back(queued);
        } else {
            let index = output
                .backpressured
                .iter()
                .position(|queued| queued.retry_at > retry_at)
                .expect("a later backpressure deadline exists");
            output.backpressured.insert(index, queued);
        }
        self.active = false;
    }

    pub(super) fn requeue_native_backpressured(
        self,
        medium: smoltcp::phy::Medium,
        meta: PacketMeta,
        frame: Vec<u8>,
        retry_at: smoltcp::time::Instant,
    ) {
        self.requeue_backpressured(
            LocalOutputPacket {
                medium,
                meta,
                disposition: LocalOutputDisposition::NativeOwner,
                frame,
            },
            retry_at,
        );
    }

    pub(super) fn requeue_deferred(
        self,
        packet: LocalOutputPacket,
        retry_at: smoltcp::time::Instant,
        probe_sent: bool,
    ) -> Result<(), LocalOutputPacket> {
        self.commit_deferred_packet(packet, retry_at, probe_sent, true)
    }

    fn commit_packet(mut self, packet: LocalOutputPacket) {
        debug_assert_eq!(packet.frame.capacity(), self.bytes);
        let mut output = self.output.lock();
        output.reserved_frames -= 1;
        output.reserved_bytes -= self.bytes;
        output.frames += 1;
        output.bytes += self.bytes;
        output.packets.push_back(packet);
        self.active = false;
    }

    pub(super) fn commit_deferred_packet(
        mut self,
        packet: LocalOutputPacket,
        retry_at: smoltcp::time::Instant,
        probe_sent: bool,
        advance_existing_probe: bool,
    ) -> Result<(), LocalOutputPacket> {
        let LocalOutputDisposition::Routed { .. } = packet.disposition else {
            return Err(packet);
        };
        let mut output = self.output.lock();
        output.deferred_routes.try_enqueue(
            packet,
            self.bytes,
            retry_at,
            probe_sent,
            advance_existing_probe,
            LocalInputQueue::DEFERRED_ROUTE_LIMITS,
        )?;
        output.reserved_frames -= 1;
        output.reserved_bytes -= self.bytes;
        output.frames += 1;
        output.bytes += self.bytes;
        self.active = false;
        Ok(())
    }

    /// Atomically join an existing neighbor-resolution bucket after routing
    /// has selected the actual egress interface.
    pub(super) fn commit_existing_deferred(
        mut self,
        packet: LocalOutputPacket,
    ) -> ExistingDeferredCommit<'a> {
        let LocalOutputDisposition::Routed { .. } = packet.disposition else {
            return ExistingDeferredCommit::Missing(packet, self);
        };
        debug_assert_eq!(packet.frame.capacity(), self.bytes);
        let mut output = self.output.lock();
        let retry_at = match output.deferred_routes.try_join(
            packet,
            self.bytes,
            LocalInputQueue::DEFERRED_ROUTE_LIMITS,
        ) {
            JoinDeferredResult::Queued(retry_at) => retry_at,
            JoinDeferredResult::Missing(packet) => {
                drop(output);
                return ExistingDeferredCommit::Missing(packet, self);
            }
            JoinDeferredResult::Full(packet) => {
                drop(output);
                return ExistingDeferredCommit::Full(packet, self);
            }
        };
        output.reserved_frames -= 1;
        output.reserved_bytes -= self.bytes;
        output.frames += 1;
        output.bytes += self.bytes;
        self.active = false;
        ExistingDeferredCommit::Queued(retry_at)
    }

    pub(super) fn finish_deferred_probe(
        mut self,
        packet: LocalOutputPacket,
        key: DeferredRouteKey,
        retry_at: smoltcp::time::Instant,
        probe_sent: bool,
    ) -> Result<bool, LocalOutputPacket> {
        debug_assert_eq!(packet.frame.capacity(), self.bytes);
        let mut output = self.output.lock();
        let LocalOutputQueueState {
            packets,
            deferred_routes,
            ..
        } = &mut *output;
        let resolved =
            deferred_routes.finish_probe(packet, self.bytes, key, retry_at, probe_sent, packets)?;
        output.reserved_frames -= 1;
        output.reserved_bytes -= self.bytes;
        output.frames += 1;
        output.bytes += self.bytes;
        self.active = false;
        Ok(resolved)
    }
}

impl Drop for LocalOutputReservation<'_> {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        let mut output = self.output.lock();
        output.reserved_frames -= 1;
        output.reserved_bytes -= self.bytes;
    }
}

impl LocalInputQueue {
    const MAX_FRAMES: usize = 1024;
    const MAX_BYTES: usize = 4 * 1024 * 1024;
    const MAX_DEFERRED_FRAMES_PER_NEIGHBOR: usize = 64;
    const MAX_DEFERRED_BYTES_PER_NEIGHBOR: usize = 256 * 1024;
    const DEFERRED_ROUTE_LIMITS: DeferredRouteLimits = DeferredRouteLimits {
        frames: Self::MAX_DEFERRED_FRAMES_PER_NEIGHBOR,
        bytes: Self::MAX_DEFERRED_BYTES_PER_NEIGHBOR,
    };
    const MAX_NEIGHBOR_PROBES: u8 = 3;
    const MAX_SCRATCH_FRAMES: usize = 64;
    const MAX_SCRATCH_BYTES: usize = 256 * 1024;

    pub(super) fn new() -> Self {
        Self {
            state: SpinLock::new(LocalInputQueueState {
                packets: VecDeque::new(),
                bytes: 0,
            }),
            response_scratch: SpinLock::new(LocalOutputScratchPool::default()),
            output: SpinLock::new(LocalOutputQueueState::default()),
            output_draining: AtomicBool::new(false),
        }
    }

    pub(super) fn enqueue(&self, packet: LocalInputPacket) -> Result<(), SystemError> {
        let mut state = self.state.lock();
        if state.packets.len() >= Self::MAX_FRAMES
            || state.bytes.saturating_add(packet.len()) > Self::MAX_BYTES
        {
            return Err(SystemError::ENOBUFS);
        }
        state
            .packets
            .try_reserve(1)
            .map_err(|_| SystemError::ENOMEM)?;
        state.bytes += packet.len();
        state.packets.push_back(packet);
        Ok(())
    }

    pub(super) fn pop(&self) -> Option<LocalInputPacket> {
        let mut state = self.state.lock();
        let packet = state.packets.pop_front()?;
        state.bytes -= packet.len();
        Some(packet)
    }

    pub(super) fn is_empty(&self) -> bool {
        self.state.lock().packets.is_empty()
    }

    pub(super) fn reserve_output(&self) -> Option<LocalOutputReservation<'_>> {
        let mut output = self.output.lock();
        if output.frames.saturating_add(output.reserved_frames) >= Self::MAX_FRAMES {
            return None;
        }
        let additional = output
            .frames
            .saturating_add(output.reserved_frames)
            .saturating_add(1)
            .saturating_sub(output.packets.len());
        output.packets.try_reserve(additional).ok()?;
        let additional_backpressured = output
            .frames
            .saturating_add(output.reserved_frames)
            .saturating_add(1)
            .saturating_sub(output.backpressured.len());
        output
            .backpressured
            .try_reserve(additional_backpressured)
            .ok()?;
        output.reserved_frames += 1;
        Some(LocalOutputReservation {
            output: &self.output,
            bytes: 0,
            active: true,
        })
    }

    pub(super) fn pop_ready_output(
        &self,
        now: smoltcp::time::Instant,
        prefer_deferred: bool,
    ) -> LocalOutputPop<'_> {
        let mut output = self.output.lock();
        while output
            .backpressured
            .front()
            .is_some_and(|queued| queued.retry_at <= now)
        {
            let queued = output
                .backpressured
                .pop_front()
                .expect("front was observed above");
            output.packets.push_back(queued.packet);
        }
        // Give one due neighbor bucket priority per drain round. The rest of
        // the round preserves FIFO service for ready output.
        let take_deferred =
            (prefer_deferred || output.packets.is_empty()) && output.deferred_routes.has_due(now);
        let (packet, probe_key) = if take_deferred {
            let LocalOutputQueueState {
                packets,
                deferred_routes,
                ..
            } = &mut *output;
            let (packet, key) = deferred_routes
                .pop_due(now, Self::MAX_NEIGHBOR_PROBES, packets)
                .expect("a due deferred bucket was observed");
            (Some(packet), key)
        } else if let Some(packet) = output.packets.pop_front() {
            (Some(packet), None)
        } else {
            (None, None)
        };
        if let Some(packet) = packet {
            let bytes = packet.frame.capacity();
            output.frames -= 1;
            output.bytes -= bytes;
            output.reserved_frames += 1;
            output.reserved_bytes += bytes;
            return LocalOutputPop::Ready(
                packet,
                LocalOutputReservation {
                    output: &self.output,
                    bytes,
                    active: true,
                },
                probe_key,
            );
        }
        match output
            .deferred_routes
            .next_retry()
            .into_iter()
            .chain(output.backpressured.front().map(|queued| queued.retry_at))
            .min()
        {
            Some(retry_at) => LocalOutputPop::DeferredUntil(retry_at),
            None => LocalOutputPop::Empty,
        }
    }

    pub(super) fn has_output(&self) -> bool {
        let output = self.output.lock();
        !output.packets.is_empty()
            || !output.backpressured.is_empty()
            || !output.deferred_routes.is_empty()
    }

    /// Whether an output packet can be claimed in the current poll round.
    /// Future retry state still keeps the queue non-empty, but must not steal
    /// NAPI budget from runnable ingress before its deadline.
    pub(super) fn has_ready_output(&self, now: smoltcp::time::Instant) -> bool {
        let output = self.output.lock();
        !output.packets.is_empty()
            || output
                .backpressured
                .front()
                .is_some_and(|queued| queued.retry_at <= now)
            || output.deferred_routes.has_due(now)
    }

    pub(super) fn release_backpressured_outputs(&self) -> bool {
        let mut output = self.output.lock();
        if output.backpressured.is_empty() {
            return false;
        }
        while let Some(queued) = output.backpressured.pop_front() {
            output.packets.push_back(queued.packet);
        }
        true
    }

    pub(super) fn has_deferred_output(&self) -> bool {
        !self.output.lock().deferred_routes.is_empty()
    }

    pub(super) fn release_resolved_outputs(
        &self,
        mut is_resolved: impl FnMut(smoltcp::wire::IpAddress) -> bool,
    ) {
        let mut output = self.output.lock();
        let LocalOutputQueueState {
            packets,
            deferred_routes,
            ..
        } = &mut *output;
        deferred_routes.release_resolved(&mut is_resolved, packets);
    }

    pub(super) fn release_neighbor(&self, oif: u32, next_hop: smoltcp::wire::IpAddress) -> bool {
        let mut output = self.output.lock();
        let LocalOutputQueueState {
            packets,
            deferred_routes,
            ..
        } = &mut *output;
        deferred_routes.release_neighbor(DeferredRouteKey { oif, next_hop }, packets)
    }

    pub(super) fn complete_deferred_probe_success(&self, key: DeferredRouteKey) {
        let mut output = self.output.lock();
        let LocalOutputQueueState {
            packets,
            deferred_routes,
            ..
        } = &mut *output;
        deferred_routes.complete_probe_success(key, packets);
    }

    /// Fail only the in-flight representative. Other packets in this bucket
    /// may still be valid and remain eligible for independent processing.
    pub(super) fn complete_deferred_packet_failure(&self, key: DeferredRouteKey) {
        let mut output = self.output.lock();
        let LocalOutputQueueState {
            packets,
            deferred_routes,
            ..
        } = &mut *output;
        deferred_routes.complete_packet_failure(key, packets);
    }

    pub(super) fn clear_routed_if_idle(
        &self,
        routed: &AtomicBool,
        bound_socket_count: &AtomicUsize,
        routed_fragments_pending: bool,
    ) {
        // Serialize the empty observation with both enqueue paths. If an
        // ingress enqueue races before these locks it is observed; if it races
        // afterwards it republishes `routed` before scheduling the poller.
        let input = self.state.lock();
        let output = self.output.lock();
        if input.packets.is_empty()
            && output.packets.is_empty()
            && output.backpressured.is_empty()
            && output.deferred_routes.is_empty()
            && output.reserved_frames == 0
            && bound_socket_count.load(Ordering::Acquire) == 0
            && !routed_fragments_pending
        {
            routed.store(false, Ordering::Release);
        }
    }

    pub(super) fn try_begin_output_drain(&self) -> Option<LocalOutputDrainGuard<'_>> {
        self.output_draining
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .ok()?;
        Some(LocalOutputDrainGuard {
            draining: &self.output_draining,
            active: true,
        })
    }

    pub(super) fn finish_output_drain(&self, guard: LocalOutputDrainGuard<'_>) -> bool {
        guard.finish_and_has_output(&self.output)
    }

    pub(super) fn recycle_output(&self, frame: Vec<u8>) {
        Self::recycle_scratch(&self.response_scratch, frame);
    }

    pub(super) fn recycle_scratch(pool: &SpinLock<LocalOutputScratchPool>, mut frame: Vec<u8>) {
        frame.clear();
        let mut pooled = pool.lock();
        if pooled.buffers.len() >= Self::MAX_SCRATCH_FRAMES
            || pooled.bytes.saturating_add(frame.capacity()) > Self::MAX_SCRATCH_BYTES
            || pooled.buffers.try_reserve(1).is_err()
        {
            return;
        }
        pooled.bytes += frame.capacity();
        pooled.buffers.push(frame);
    }
}
