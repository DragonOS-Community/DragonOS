//! Socket-wide UDP receive queue.
//!
//! Linux demultiplexes packets from every netdevice into one `sk_receive_queue`.
//! Keeping this queue above DragonOS's per-interface smoltcp instances gives a
//! wildcard UDP socket the same ownership and buffer-accounting semantics.

use alloc::{collections::VecDeque, vec::Vec};

use smoltcp::{
    phy::PacketMeta,
    wire::{IpAddress, IpEndpoint},
};

use crate::libs::mutex::Mutex;

#[derive(Clone, Copy, Debug)]
struct ConnectedPeer {
    remote: IpEndpoint,
    local: Option<IpAddress>,
}

#[derive(Debug)]
pub(super) struct UdpRxDatagram {
    pub source: IpEndpoint,
    pub destination: IpAddress,
    pub ifindex: i32,
    pub meta: PacketMeta,
    pub payload: Vec<u8>,
    accounted_bytes: usize,
}

#[derive(Debug)]
struct QueueState {
    packets: VecDeque<UdpRxDatagram>,
    bytes: usize,
    capacity: usize,
    connected: Option<ConnectedPeer>,
    accepting: bool,
    binding_generation: u64,
}

#[derive(Debug)]
pub(super) struct UdpReceiveQueue {
    state: Mutex<QueueState>,
}

impl UdpReceiveQueue {
    pub fn new(capacity: usize) -> Self {
        Self {
            state: Mutex::new(QueueState {
                packets: VecDeque::new(),
                bytes: 0,
                capacity,
                connected: None,
                accepting: true,
                binding_generation: 0,
            }),
        }
    }

    pub fn set_capacity(&self, capacity: usize) {
        self.state.lock().capacity = capacity;
    }

    pub fn capacity(&self) -> usize {
        self.state.lock().capacity
    }

    pub fn begin_binding(&self) -> u64 {
        let mut state = self.state.lock();
        state.accepting = true;
        state.binding_generation = state.binding_generation.wrapping_add(1);
        state.binding_generation
    }

    pub fn invalidate_binding(&self) {
        let mut state = self.state.lock();
        state.binding_generation = state.binding_generation.wrapping_add(1);
    }

    pub fn connect(&self, remote: IpEndpoint, local: Option<IpAddress>) {
        self.state.lock().connected = Some(ConnectedPeer { remote, local });
    }

    pub fn disconnect(&self) {
        self.state.lock().connected = None;
    }

    pub fn close(&self) {
        let mut state = self.state.lock();
        state.accepting = false;
        state.binding_generation = state.binding_generation.wrapping_add(1);
        state.connected = None;
        state.packets.clear();
        state.bytes = 0;
    }

    /// Returns whether this binding accepts the flow and whether the socket is
    /// connected. The latter is part of Linux UDP's unicast lookup score.
    pub fn ingress_match(
        &self,
        generation: u64,
        source: IpEndpoint,
        destination: IpAddress,
    ) -> Option<bool> {
        let state = self.state.lock();
        if !state.accepting
            || state.binding_generation != generation
            || !peer_accepts(state.connected, source, destination)
        {
            return None;
        }
        Some(state.connected.is_some())
    }

    /// Enqueues one validated datagram. Returns whether the queue changed from
    /// empty to non-empty; callers use that edge for lock-safe readiness wakeups.
    pub fn enqueue(
        &self,
        generation: u64,
        source: IpEndpoint,
        destination: IpAddress,
        ifindex: i32,
        meta: PacketMeta,
        payload: &[u8],
    ) -> Option<bool> {
        let accounted_bytes = payload
            .len()
            .saturating_add(core::mem::size_of::<UdpRxDatagram>());
        {
            let state = self.state.lock();
            if !state.accepting
                || state.binding_generation != generation
                || !peer_accepts(state.connected, source, destination)
                || accounted_bytes > state.capacity.saturating_sub(state.bytes)
            {
                return None;
            }
        }
        // Perform the payload allocation before taking the queue lock. The
        // callback runs in the interface poll path, so the serialized commit
        // below must stay short and must revalidate all mutable admission state.
        let mut owned = Vec::new();
        if owned.try_reserve_exact(payload.len()).is_err() {
            return None;
        }
        owned.extend_from_slice(payload);

        let mut state = self.state.lock();
        if !state.accepting || !peer_accepts(state.connected, source, destination) {
            return None;
        }
        if state.binding_generation != generation {
            return None;
        }
        // Linux charges receive-queue metadata as well as payload. Accounting
        // a fixed per-datagram cost also bounds a flood of zero-length UDP.
        if accounted_bytes > state.capacity.saturating_sub(state.bytes) {
            return None;
        }
        if state.packets.try_reserve(1).is_err() {
            return None;
        }
        let was_empty = state.packets.is_empty();
        state.bytes += accounted_bytes;
        state.packets.push_back(UdpRxDatagram {
            source,
            destination,
            ifindex,
            meta,
            payload: owned,
            accounted_bytes,
        });
        Some(was_empty)
    }

    pub fn is_empty(&self) -> bool {
        self.state.lock().packets.is_empty()
    }

    pub fn first_len(&self) -> Option<usize> {
        self.state
            .lock()
            .packets
            .front()
            .map(|packet| packet.payload.len())
    }

    pub fn recv(
        &self,
        buf: &mut [u8],
        peek: bool,
    ) -> Option<(usize, usize, IpEndpoint, IpAddress, i32, PacketMeta)> {
        let mut state = self.state.lock();
        let packet = state.packets.front()?;
        let copy_len = core::cmp::min(buf.len(), packet.payload.len());
        buf[..copy_len].copy_from_slice(&packet.payload[..copy_len]);
        let result = (
            copy_len,
            packet.payload.len(),
            packet.source,
            packet.destination,
            packet.ifindex,
            packet.meta,
        );
        if !peek {
            let packet = state.packets.pop_front().unwrap();
            state.bytes -= packet.accounted_bytes;
        }
        Some(result)
    }
}

#[inline]
fn peer_accepts(peer: Option<ConnectedPeer>, source: IpEndpoint, destination: IpAddress) -> bool {
    peer.is_none_or(|peer| {
        peer.remote == source && peer.local.is_none_or(|local| local == destination)
    })
}
