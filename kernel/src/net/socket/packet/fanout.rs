//! AF_PACKET `PACKET_FANOUT` group demultiplexing.
//!
//! A fanout group fans matching ingress frames out to exactly one of its member
//! sockets, selected by the group's distribution algorithm (HASH/LB/CPU/RND/
//! ROLLOVER). Members are registered with the owning `NetNamespace` and use the
//! same RCU copy-on-write publication pattern as the broadcast packet-socket
//! registry, so the NAPI deliver path stays lock-free on the read side.

use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::Ordering;
use system_error::SystemError;

use jhash::jhash2;

use crate::libs::mutex::Mutex;
use crate::rcu::RcuArcSlot;

use super::rx::l2_ethertype_l3_offset;
use super::uapi::{eth_protocol, fanout_flag, fanout_mode};
use super::{PacketIngressMetadata, PacketSocket, PacketSocketType};

const IPPROTO_TCP: u8 = 6;
const IPPROTO_UDP: u8 = 17;

/// Supported fanout distribution algorithms.
///
/// QM(5)/CBPF(6)/EBPF(7) require NIC RSS queue mapping / BPF infrastructure
/// that DragonOS does not yet provide, so `FanoutMode::from_raw` rejects them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FanoutMode {
    /// 0 — flow hash of the L3/L4 4-tuple modulo the member count.
    Hash,
    /// 1 — round-robin via an atomic counter.
    Lb,
    /// 2 — receiving CPU id modulo the member count.
    Cpu,
    /// 3 — start from the round-robin base and roll over filled members.
    Rollover,
    /// 4 — uniformly random member selection.
    Rnd,
}

impl FanoutMode {
    /// Parse the low 7 bits of `type_flags`. Returns `EINVAL` for unsupported
    /// modes (QM/CBPF/EBPF).
    fn from_raw(raw: u32) -> Result<Self, SystemError> {
        match raw {
            fanout_mode::PACKET_FANOUT_HASH => Ok(Self::Hash),
            fanout_mode::PACKET_FANOUT_LB => Ok(Self::Lb),
            fanout_mode::PACKET_FANOUT_CPU => Ok(Self::Cpu),
            fanout_mode::PACKET_FANOUT_ROLLOVER => Ok(Self::Rollover),
            fanout_mode::PACKET_FANOUT_RND => Ok(Self::Rnd),
            // PACKET_FANOUT_QM / CBPF / EBPF are unsupported.
            _ => Err(SystemError::EINVAL),
        }
    }

    fn as_raw(self) -> u32 {
        match self {
            Self::Hash => fanout_mode::PACKET_FANOUT_HASH,
            Self::Lb => fanout_mode::PACKET_FANOUT_LB,
            Self::Cpu => fanout_mode::PACKET_FANOUT_CPU,
            Self::Rollover => fanout_mode::PACKET_FANOUT_ROLLOVER,
            Self::Rnd => fanout_mode::PACKET_FANOUT_RND,
        }
    }
}

/// Join-time parameter bundle for `NetNamespace::fanout_group_join`.
///
/// Grouping these values keeps that entry point under clippy's
/// `too_many_arguments` threshold while letting the packet module hand the
/// netns a single, self-describing value.
#[derive(Debug, Clone, Copy)]
pub(crate) struct FanoutJoinParams {
    /// Explicit group id requested by the socket (ignored when `unique`).
    pub id_req: u16,
    /// `true` for `PACKET_FANOUT_FLAG_UNIQUEID`: allocate a fresh group.
    pub unique: bool,
    /// Distribution algorithm for the group.
    pub mode: FanoutMode,
    /// Raw `PACKET_FANOUT_FLAG_*` bits requested by the creator.
    pub flags: u16,
    /// Joining socket's type / binding filter — enforced on every join.
    pub sock_type: PacketSocketType,
    pub bound_ifindex: u32,
    pub bound_protocol: u16,
}

/// RCU-published member list of a fanout group (mirrors the broadcast
/// `PacketSocketRegistrySnapshot` COW pattern).
#[derive(Debug, Default)]
struct FanoutMemberSnapshot {
    members: Vec<Weak<PacketSocket>>,
}

/// A fanout group: an ordered set of sockets sharing a distribution algorithm.
///
/// All members are guaranteed (at join time) to share the same socket type and
/// `(ifindex, protocol)` receive filter, so any demux choice yields identical
/// `deliver_from_iface` filtering.
#[derive(Debug)]
pub(crate) struct FanoutGroup {
    pub id: u16,
    pub mode: FanoutMode,
    /// Raw `PACKET_FANOUT_FLAG_*` bits requested by the creator.
    pub flags: u16,
    /// Creator's socket type / binding filter — enforced on every join.
    sock_type: PacketSocketType,
    bound_ifindex: u32,
    bound_protocol: u16,
    /// Random HASH-mode seed to avoid flow polarization across groups.
    hash_seed: u32,
    /// Member list, RCU COW (mirrors `NetNamespace::packet_sockets`).
    members: RcuArcSlot<FanoutMemberSnapshot>,
    /// Serializes copy-on-write member-list updates.
    members_writer: Mutex<()>,
    /// LB / ROLLOVER round-robin counter (atomic, multi-core safe).
    rr_counter: core::sync::atomic::AtomicU32,
}

impl FanoutGroup {
    pub(crate) fn new(id: u16, params: FanoutJoinParams) -> Arc<Self> {
        // A per-group random seed prevents flows from polarizing onto the same
        // member across groups that happen to share a hash function.
        let hash_seed = crate::arch::rand::rand() as u32;
        Arc::new(Self {
            id,
            mode: params.mode,
            flags: params.flags,
            sock_type: params.sock_type,
            bound_ifindex: params.bound_ifindex,
            bound_protocol: params.bound_protocol,
            hash_seed,
            members: RcuArcSlot::new(Arc::new(FanoutMemberSnapshot::default())),
            members_writer: Mutex::new(()),
            rr_counter: core::sync::atomic::AtomicU32::new(0),
        })
    }

    /// Whether a joining socket matches this group's creator profile.
    pub(crate) fn matches(
        &self,
        sock_type: PacketSocketType,
        bound_ifindex: u32,
        bound_protocol: u16,
    ) -> bool {
        self.sock_type == sock_type
            && self.bound_ifindex == bound_ifindex
            && self.bound_protocol == bound_protocol
    }

    /// Publish a new member-list snapshot containing `socket` (RCU COW).
    pub(crate) fn add_member(&self, socket: Weak<PacketSocket>) {
        let _guard = self.members_writer.lock();
        let current = self.members.load();
        let mut members = current.members.clone();
        // Drop already-dead weak refs while we hold the write-side clone.
        members.retain(|entry| entry.strong_count() != 0);
        if !members.iter().any(|entry| Weak::ptr_eq(entry, &socket)) {
            members.push(socket);
        }
        self.members
            .store_deferred(Arc::new(FanoutMemberSnapshot { members }));
    }

    /// Publish a new member-list snapshot without `socket` (RCU COW).
    pub(crate) fn remove_member(&self, socket: &Weak<PacketSocket>) {
        let _guard = self.members_writer.lock();
        let current = self.members.load();
        let mut members = current.members.clone();
        members.retain(|entry| entry.strong_count() != 0 && !Weak::ptr_eq(entry, socket));
        self.members
            .store_deferred(Arc::new(FanoutMemberSnapshot { members }));
    }

    /// Count live members. Called only under the netns `fanout_groups_writer`
    /// lock, which blocks concurrent `add_member`/`remove_member`, so the RCU
    /// snapshot read here is stable for the empty-group teardown decision.
    pub(crate) fn live_count(&self) -> usize {
        let current = self.members.load();
        current
            .members
            .iter()
            .filter(|entry| entry.strong_count() != 0)
            .count()
    }

    /// Deliver one copy of the frame to a single member selected by the
    /// distribution algorithm. Returns `true` when the member list contained
    /// dead weak refs (lazy cleanup hint for the netns).
    pub(crate) fn deliver(
        &self,
        ingress: PacketIngressMetadata,
        frame: &[u8],
    ) -> bool {
        let snapshot = self.members.load();

        // First pass: count live members without any heap allocation.
        let mut stale = false;
        let mut live_count = 0usize;
        for entry in snapshot.members.iter() {
            if entry.strong_count() != 0 {
                live_count += 1;
            } else {
                stale = true;
            }
        }
        if live_count == 0 {
            return stale;
        }

        let base = match self.mode {
            FanoutMode::Hash => flow_hash(frame, self.hash_seed) as usize % live_count,
            FanoutMode::Lb | FanoutMode::Rollover => {
                self.rr_counter.fetch_add(1, Ordering::Relaxed) as usize % live_count
            }
            FanoutMode::Cpu => crate::arch::cpu::current_cpu_id().data() as usize % live_count,
            FanoutMode::Rnd => crate::arch::rand::rand() % live_count,
        };
        // ROLLOVER semantics: mode == Rollover always rolls over filled
        // backlogs; FLAG_ROLLOVER layers the same fallback onto other modes.
        let rollover = self.mode == FanoutMode::Rollover
            || (self.flags & fanout_flag::PACKET_FANOUT_FLAG_ROLLOVER != 0);

        // Second pass: find and deliver to the selected socket, rolling over
        // filled backlogs. Each candidate is upgraded lazily —
        // strong_count()!=0 guarantees upgrade() succeeds, so no heap
        // allocation occurs.
        for offset in 0..live_count {
            let target = (base + offset) % live_count;
            let Some(socket) = nth_live_member(&snapshot.members, target) else {
                continue;
            };
            if !rollover || socket.rx_has_room() {
                socket.deliver(ingress, frame);
                return stale;
            }
        }
        // All members full (only reachable on the rollover path): hand the
        // frame to `base` so `deliver_from_iface` records the drop statistic.
        if let Some(socket) = nth_live_member(&snapshot.members, base) {
            socket.deliver(ingress, frame);
        }
        stale
    }
}

/// Upgrade the `target`-th live member (0-indexed among live entries) of a
/// fanout snapshot. `strong_count()!=0` guarantees `upgrade()` succeeds, so
/// this performs no heap allocation.
fn nth_live_member(members: &[Weak<PacketSocket>], target: usize) -> Option<Arc<PacketSocket>> {
    let mut seen = 0usize;
    for entry in members {
        if entry.strong_count() == 0 {
            continue;
        }
        if seen == target {
            return entry.upgrade();
        }
        seen += 1;
    }
    None
}

/// Compute a flow hash over the L3/L4 4-tuple for HASH-mode demux.
///
/// Non-IP frames hash to 0 (all land on the base member — degraded but
/// lossless). VLAN tags are skipped so tagged and untagged frames of the same
/// flow hash identically.
fn flow_hash(frame: &[u8], seed: u32) -> u32 {
    let Some((ether, l3_off)) = l2_ethertype_l3_offset(frame) else {
        return 0;
    };
    let l3 = if frame.len() > l3_off {
        &frame[l3_off..]
    } else {
        return 0;
    };

    if ether == eth_protocol::ETH_P_IP {
        // IPv4
        if l3.len() < 20 {
            return 0;
        }
        if l3[0] >> 4 != 4 {
            return 0;
        }
        let ihl = (l3[0] as usize & 0xf) * 4;
        if ihl < 20 || l3.len() < ihl {
            return 0;
        }
        let src = u32::from_be_bytes([l3[12], l3[13], l3[14], l3[15]]);
        let dst = u32::from_be_bytes([l3[16], l3[17], l3[18], l3[19]]);
        let proto = l3[9];
        let (sp, dp) = l4_ports(&l3[ihl..], proto);
        let words = [src, dst, sp as u32, dp as u32];
        jhash2(&words, seed)
    } else if ether == eth_protocol::ETH_P_IPV6 {
        // IPv6
        if l3.len() < 40 {
            return 0;
        }
        let mut src = [0u32; 4];
        let mut dst = [0u32; 4];
        for i in 0..4 {
            src[i] =
                u32::from_be_bytes([l3[8 + i * 4], l3[9 + i * 4], l3[10 + i * 4], l3[11 + i * 4]]);
            dst[i] = u32::from_be_bytes([
                l3[24 + i * 4],
                l3[25 + i * 4],
                l3[26 + i * 4],
                l3[27 + i * 4],
            ]);
        }
        let next_header = l3[6];
        let (sp, dp) = l4_ports(&l3[40..], next_header);
        let words = [
            src[0], src[1], src[2], src[3], dst[0], dst[1], dst[2], dst[3], sp as u32, dp as u32,
        ];
        jhash2(&words, seed)
    } else {
        0
    }
}

/// Extract the source/destination ports from an L4 segment for TCP/UDP.
fn l4_ports(l4: &[u8], proto: u8) -> (u16, u16) {
    if (proto == IPPROTO_TCP || proto == IPPROTO_UDP) && l4.len() >= 4 {
        let sp = u16::from_be_bytes([l4[0], l4[1]]);
        let dp = u16::from_be_bytes([l4[2], l4[3]]);
        (sp, dp)
    } else {
        (0, 0)
    }
}

impl PacketSocket {
    /// `setsockopt(SOL_PACKET, PACKET_FANOUT, val)` entry point.
    ///
    /// Parses the option value, validates mode/flags, then delegates the
    /// locked registry work — including the already-joined (`EBUSY`) check —
    /// to the owning netns so that the check, "publish member", and "set
    /// fanout field" all happen atomically under the netns writer lock.
    pub(crate) fn join_fanout(&self, val: i32) -> Result<(), SystemError> {
        let v = val as u32;
        let id_req = (v & 0xffff) as u16;
        let type_flags = v >> 16;
        let mode = FanoutMode::from_raw(type_flags & 0x7f)?;
        let flags = (type_flags & !0x7f) as u16;

        // Only ROLLOVER and UNIQUEID flags are supported. DEFRAG/IGNORE_OUTGOING
        // need infrastructure DragonOS lacks; reject explicitly rather than
        // silently ignoring them.
        const ALLOWED_FLAGS: u16 =
            fanout_flag::PACKET_FANOUT_FLAG_ROLLOVER | fanout_flag::PACKET_FANOUT_FLAG_UNIQUEID;
        if flags & !ALLOWED_FLAGS != 0 {
            return Err(SystemError::EINVAL);
        }

        let unique = flags & fanout_flag::PACKET_FANOUT_FLAG_UNIQUEID != 0;
        let Some(socket) = self.self_ref.upgrade() else {
            return Err(SystemError::EINVAL);
        };
        // Extract the join profile here (inside the packet module, where the
        // fields are visible) and hand the values to the netns.
        let sock_type = self.sock_type;
        let (bound_ifindex, bound_protocol) = self.binding.load();
        self.netns.fanout_group_join(
            &socket,
            FanoutJoinParams {
                id_req,
                unique,
                mode,
                flags,
                sock_type,
                bound_ifindex,
                bound_protocol,
            },
        )
    }

    /// Leave the current fanout group (invoked from `close_binding`).
    ///
    /// Delegates entirely to the owning netns, which acquires
    /// `fanout_groups_writer` *before* taking the socket's `fanout` write lock.
    /// This keeps the lock order identical to the join path
    /// (`fanout_groups_writer → fanout.write()`), avoiding the ABBA deadlock
    /// that would arise if the leave path cleared `fanout` first.
    pub(crate) fn leave_fanout(&self) {
        self.netns.fanout_group_leave(&self.self_ref);
    }

    /// `true` if this socket currently belongs to a fanout group. Used by the
    /// broadcast deliver path to skip sockets handled by their group.
    pub(crate) fn is_fanout_member(&self) -> bool {
        self.fanout_active.load(Ordering::Acquire)
    }

    /// Record group membership. Called by the netns join path while it holds
    /// the `fanout_groups_writer` lock, so member publication and field update
    /// are atomic from a concurrent join/leave viewpoint.
    pub(crate) fn set_fanout_group(&self, group: Arc<FanoutGroup>) {
        *self.fanout.write() = Some(group);
        self.fanout_active.store(true, Ordering::Release);
    }

    /// Whether this socket currently holds a fanout group reference.
    /// Checked by the netns join path under the `fanout_groups_writer` lock
    /// for the already-joined (`EBUSY`) test.
    pub(crate) fn has_fanout_group(&self) -> bool {
        self.fanout.read().is_some()
    }

    /// Take and clear the fanout group membership, deactivating the socket.
    /// Called by the netns leave path *after* acquiring `fanout_groups_writer`,
    /// so the clear and the registry mutation share the same lock order as
    /// the join path (`fanout_groups_writer → fanout.write()`).
    pub(crate) fn clear_fanout_group(&self) -> Option<Arc<FanoutGroup>> {
        let group = self.fanout.write().take();
        if group.is_some() {
            self.fanout_active.store(false, Ordering::Release);
        }
        group
    }

    /// Current fanout membership encoded as `(id | (type_flags << 16))`, or
    /// `None` if the socket has not joined a group.
    pub(crate) fn fanout_getsockopt_value(&self) -> Option<u32> {
        let group = self.fanout.read();
        let group = group.as_ref()?;
        let type_flags = group.mode.as_raw() | group.flags as u32;
        Some(group.id as u32 | (type_flags << 16))
    }

    /// `true` when the receive buffer still has room for another frame. Mirrors
    /// the admission check in `deliver_from_iface` (rx.rs) so the ROLLOVER
    /// fallback can skip a backlogged member identically.
    pub(super) fn rx_has_room(&self) -> bool {
        self.rx_buffer_bytes.load(Ordering::Acquire)
            < self.recv_buffer_bytes.load(Ordering::Relaxed)
    }
}
