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
        frame.extend_from_slice(&[0x08, 0x00]);
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

#[derive(Debug)]
pub(super) struct DeferredRouteBucket {
    pub(super) oif: u32,
    pub(super) next_hop: smoltcp::wire::Ipv4Address,
    pub(super) retry_at: smoltcp::time::Instant,
    pub(super) probes: u8,
    pub(super) probe_in_flight: bool,
    pub(super) probe_bytes: usize,
    pub(super) resolved: bool,
    pub(super) packets: VecDeque<LocalOutputPacket>,
    pub(super) bytes: usize,
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
        next_hop: smoltcp::wire::Ipv4Address,
        ip_mtu: usize,
    },
    Drop,
}

impl LocalOutputDisposition {
    pub(super) const DROP_CONTEXT: u64 = 0;
    const LOCAL_CONTEXT: u64 = 1 << 63;
    // Reserved opaque smoltcp fragment context. Valid ifindices use the
    // positive i32 range, so neither routed nor local encodings can collide
    // with the all-ones value.
    pub(super) const NATIVE_CONTEXT: u64 = u64::MAX;

    pub(super) fn routed_context(oif: u32, next_hop: smoltcp::wire::Ipv4Address) -> u64 {
        debug_assert_ne!(oif, 0);
        debug_assert_eq!(oif & (1 << 31), 0);
        ((oif as u64) << 32) | u32::from_be_bytes(next_hop.octets()) as u64
    }

    pub(super) fn local_context(oif: u32) -> u64 {
        debug_assert_ne!(oif, 0);
        debug_assert_eq!(oif & (1 << 31), 0);
        Self::LOCAL_CONTEXT | ((oif as u64) << 32)
    }

    pub(super) fn from_context(context: u64, ip_mtu: usize) -> Self {
        if context == Self::NATIVE_CONTEXT {
            return Self::NativeOwner;
        }
        if context & Self::LOCAL_CONTEXT != 0 {
            return Self::Local {
                oif: ((context >> 32) as u32) & !(1 << 31),
                ip_mtu,
            };
        }
        let oif = (context >> 32) as u32;
        if oif == 0 {
            return Self::Drop;
        }
        let octets = (context as u32).to_be_bytes();
        Self::Routed {
            oif,
            next_hop: smoltcp::wire::Ipv4Address::new(octets[0], octets[1], octets[2], octets[3]),
            ip_mtu,
        }
    }
}

#[derive(Debug, Default)]
pub(super) struct LocalOutputQueueState {
    pub(super) packets: VecDeque<LocalOutputPacket>,
    pub(super) backpressured: VecDeque<BackpressuredLocalOutput>,
    pub(super) deferred_routes: Vec<DeferredRouteBucket>,
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

#[derive(Clone, Copy)]
pub(super) struct DeferredRouteKey {
    pub(super) oif: u32,
    pub(super) next_hop: smoltcp::wire::Ipv4Address,
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
        let LocalOutputDisposition::Routed { oif, next_hop, .. } = packet.disposition else {
            return Err(packet);
        };
        let mut output = self.output.lock();
        let bucket_index = output
            .deferred_routes
            .iter()
            .position(|bucket| bucket.oif == oif && bucket.next_hop == next_hop);
        if let Some(index) = bucket_index {
            let bucket = &mut output.deferred_routes[index];
            if bucket.packets.len() + usize::from(bucket.probe_in_flight)
                >= LocalInputQueue::MAX_DEFERRED_FRAMES_PER_NEIGHBOR
                || bucket
                    .bytes
                    .saturating_add(bucket.probe_bytes)
                    .saturating_add(self.bytes)
                    > LocalInputQueue::MAX_DEFERRED_BYTES_PER_NEIGHBOR
                || bucket.packets.try_reserve(1).is_err()
            {
                drop(output);
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
            bucket.bytes += self.bytes;
            bucket.packets.push_back(packet);
        } else {
            if output.deferred_routes.try_reserve(1).is_err() {
                drop(output);
                return Err(packet);
            }
            let mut packets = VecDeque::new();
            if packets.try_reserve(1).is_err() {
                drop(output);
                return Err(packet);
            }
            packets.push_back(packet);
            output.deferred_routes.push(DeferredRouteBucket {
                oif,
                next_hop,
                retry_at,
                probes: u8::from(probe_sent),
                probe_in_flight: false,
                probe_bytes: 0,
                resolved: false,
                packets,
                bytes: self.bytes,
            });
        }
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
        let LocalOutputDisposition::Routed { oif, next_hop, .. } = packet.disposition else {
            return ExistingDeferredCommit::Missing(packet, self);
        };
        debug_assert_eq!(packet.frame.capacity(), self.bytes);
        let mut output = self.output.lock();
        let Some(index) = output
            .deferred_routes
            .iter()
            .position(|bucket| bucket.oif == oif && bucket.next_hop == next_hop)
        else {
            drop(output);
            return ExistingDeferredCommit::Missing(packet, self);
        };
        let bucket = &mut output.deferred_routes[index];
        if bucket.packets.len() + usize::from(bucket.probe_in_flight)
            >= LocalInputQueue::MAX_DEFERRED_FRAMES_PER_NEIGHBOR
            || bucket
                .bytes
                .saturating_add(bucket.probe_bytes)
                .saturating_add(self.bytes)
                > LocalInputQueue::MAX_DEFERRED_BYTES_PER_NEIGHBOR
            || bucket.packets.try_reserve(1).is_err()
        {
            drop(output);
            return ExistingDeferredCommit::Full(packet, self);
        }
        let retry_at = bucket.retry_at;
        bucket.bytes += self.bytes;
        bucket.packets.push_back(packet);
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
        let Some(index) = output.deferred_routes.iter().position(|bucket| {
            bucket.oif == key.oif && bucket.next_hop == key.next_hop && bucket.probe_in_flight
        }) else {
            drop(output);
            return Err(packet);
        };

        let resolved = output.deferred_routes[index].resolved;
        {
            let bucket = &mut output.deferred_routes[index];
            debug_assert_eq!(bucket.probe_bytes, self.bytes);
            bucket.probe_in_flight = false;
            bucket.probe_bytes = 0;
            bucket.bytes += self.bytes;
            bucket.packets.push_front(packet);
            if !resolved {
                bucket.retry_at = retry_at;
                if probe_sent {
                    bucket.probes = bucket.probes.saturating_add(1);
                }
            }
        }

        output.reserved_frames -= 1;
        output.reserved_bytes -= self.bytes;
        output.frames += 1;
        output.bytes += self.bytes;
        if resolved {
            let mut bucket = output.deferred_routes.swap_remove(index);
            output.packets.append(&mut bucket.packets);
        }
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
        let due_index = (prefer_deferred || output.packets.is_empty()).then(|| {
            output
                .deferred_routes
                .iter()
                .position(|bucket| !bucket.probe_in_flight && bucket.retry_at <= now)
        });
        let due_index = due_index.flatten();
        // Give one due neighbor bucket priority per drain round. The rest of
        // the round preserves FIFO service for ready output.
        let take_deferred = due_index.is_some() && (prefer_deferred || output.packets.is_empty());
        let (packet, probe_key) = if take_deferred {
            let index = due_index.expect("take_deferred requires a due bucket");
            if output.deferred_routes[index].resolved
                || output.deferred_routes[index].probes >= Self::MAX_NEIGHBOR_PROBES
            {
                let mut bucket = output.deferred_routes.swap_remove(index);
                if !bucket.resolved {
                    for packet in bucket.packets.iter_mut() {
                        packet.disposition = LocalOutputDisposition::Drop;
                    }
                }
                let packet = bucket.packets.pop_front();
                output.packets.append(&mut bucket.packets);
                (packet, None)
            } else {
                let bucket = &mut output.deferred_routes[index];
                let packet = bucket
                    .packets
                    .pop_front()
                    .expect("a deferred route bucket is never empty");
                let bytes = packet.frame.capacity();
                bucket.bytes -= bytes;
                bucket.probe_in_flight = true;
                bucket.probe_bytes = bytes;
                (
                    Some(packet),
                    Some(DeferredRouteKey {
                        oif: bucket.oif,
                        next_hop: bucket.next_hop,
                    }),
                )
            }
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
            .iter()
            .filter(|bucket| !bucket.probe_in_flight)
            .map(|bucket| bucket.retry_at)
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
            || output
                .deferred_routes
                .iter()
                .any(|bucket| !bucket.probe_in_flight && bucket.retry_at <= now)
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
        mut is_resolved: impl FnMut(smoltcp::wire::Ipv4Address) -> bool,
    ) {
        let mut output = self.output.lock();
        let mut index = 0;
        while index < output.deferred_routes.len() {
            if is_resolved(output.deferred_routes[index].next_hop) {
                if output.deferred_routes[index].probe_in_flight {
                    output.deferred_routes[index].resolved = true;
                    index += 1;
                } else {
                    let mut bucket = output.deferred_routes.swap_remove(index);
                    output.packets.append(&mut bucket.packets);
                }
            } else {
                index += 1;
            }
        }
    }

    pub(super) fn release_neighbor(&self, oif: u32, next_hop: smoltcp::wire::Ipv4Address) -> bool {
        let mut output = self.output.lock();
        let Some(index) = output
            .deferred_routes
            .iter()
            .position(|bucket| bucket.oif == oif && bucket.next_hop == next_hop)
        else {
            return false;
        };
        if output.deferred_routes[index].probe_in_flight {
            output.deferred_routes[index].resolved = true;
        } else {
            let mut bucket = output.deferred_routes.swap_remove(index);
            output.packets.append(&mut bucket.packets);
        }
        true
    }

    pub(super) fn complete_deferred_probe_success(&self, key: DeferredRouteKey) {
        let mut output = self.output.lock();
        let Some(index) = output.deferred_routes.iter().position(|bucket| {
            bucket.oif == key.oif && bucket.next_hop == key.next_hop && bucket.probe_in_flight
        }) else {
            return;
        };
        let mut bucket = output.deferred_routes.swap_remove(index);
        output.packets.append(&mut bucket.packets);
    }

    /// Fail only the in-flight representative. Other packets in this bucket
    /// may still be valid and remain eligible for independent processing.
    pub(super) fn complete_deferred_packet_failure(&self, key: DeferredRouteKey) {
        let mut output = self.output.lock();
        let Some(index) = output.deferred_routes.iter().position(|bucket| {
            bucket.oif == key.oif && bucket.next_hop == key.next_hop && bucket.probe_in_flight
        }) else {
            return;
        };
        let bucket = &mut output.deferred_routes[index];
        bucket.probe_in_flight = false;
        bucket.probe_bytes = 0;
        if bucket.packets.is_empty() {
            output.deferred_routes.swap_remove(index);
        }
    }

    pub(super) fn clear_routed_if_idle(
        &self,
        routed: &AtomicBool,
        bound_socket_count: &AtomicUsize,
        deferred_close_pending: bool,
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
            && !deferred_close_pending
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
