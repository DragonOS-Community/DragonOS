//! AF_PACKET `PACKET_FANOUT` group demultiplexing.
//!
//! The owning network namespace publishes the complete plain-socket and
//! fanout-group topology in one RCU snapshot.  Group snapshots are immutable;
//! write-side membership changes create a replacement snapshot while sharing
//! the small amount of algorithm runtime state.

use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::cmp::Ordering as CmpOrdering;
use core::sync::atomic::{AtomicU32, Ordering};

use jhash::jhash2;
use system_error::SystemError;

use super::rx::packet_protocol;
use super::uapi::{eth_protocol, fanout_flag, fanout_mode};
use super::{PacketIngressMetadata, PacketSocket, PacketType};

const IPPROTO_TCP: u8 = 6;
const IPPROTO_UDP: u8 = 17;
pub(crate) const LEGACY_FANOUT_MAX_MEMBERS: usize = 256;
pub(crate) const FANOUT_MAX_MEMBERS: usize = 1 << 16;

/// One process-wide secret, matching Linux's use of a shared boot-random
/// secret for `__skb_get_hash_symmetric()` rather than a per-group seed.
static FLOW_HASH_SEED: AtomicU32 = AtomicU32::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FanoutMode {
    Hash,
    Lb,
    Cpu,
    Rollover,
    Rnd,
}

impl FanoutMode {
    fn from_raw(raw: u32) -> Result<Self, SystemError> {
        match raw {
            fanout_mode::PACKET_FANOUT_HASH => Ok(Self::Hash),
            fanout_mode::PACKET_FANOUT_LB => Ok(Self::Lb),
            fanout_mode::PACKET_FANOUT_CPU => Ok(Self::Cpu),
            fanout_mode::PACKET_FANOUT_ROLLOVER => Ok(Self::Rollover),
            fanout_mode::PACKET_FANOUT_RND => Ok(Self::Rnd),
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

#[derive(Debug, Clone, Copy)]
pub(crate) struct FanoutJoinParams {
    pub id_req: u16,
    pub unique: bool,
    pub mode: FanoutMode,
    /// Persistent group flags. `UNIQUEID` is removed while parsing because it
    /// is an allocation request, not group identity.
    pub flags: u16,
    pub bound_ifindex: u32,
    pub bound_protocol: u16,
    /// Zero selects the legacy 256-member limit. Non-zero values come from
    /// Linux's 8-byte `fanout_args` ABI and are part of group identity.
    pub max_num_members: u32,
}

#[derive(Debug)]
struct FanoutRuntime {
    rr_counter: AtomicU32,
}

impl FanoutRuntime {
    fn try_new() -> Result<Arc<Self>, SystemError> {
        Arc::try_new(Self {
            rr_counter: AtomicU32::new(0),
        })
        .map_err(|_| SystemError::ENOMEM)
    }
}

/// Linux associates rollover history with the initially selected packet
/// socket. Keep a separate cursor per primary member so HASH/LB/CPU/RND flows
/// do not perturb one another's fallback start.
#[derive(Debug)]
struct MemberRolloverState {
    cursor: AtomicU32,
}

#[derive(Debug, Clone)]
struct FanoutMember {
    socket: Weak<PacketSocket>,
    rollover: Option<Arc<MemberRolloverState>>,
}

impl FanoutMember {
    fn try_new(socket: Weak<PacketSocket>, rollover: bool) -> Result<Self, SystemError> {
        Ok(Self {
            socket,
            rollover: if rollover {
                Some(
                    Arc::try_new(MemberRolloverState {
                        cursor: AtomicU32::new(0),
                    })
                    .map_err(|_| SystemError::ENOMEM)?,
                )
            } else {
                None
            },
        })
    }

    #[inline]
    fn active_socket(&self) -> Option<Arc<PacketSocket>> {
        self.socket
            .upgrade()
            .filter(|socket| socket.is_packet_registry_active())
    }
}

/// Immutable group snapshot stored inside the netns delivery topology.
#[derive(Debug)]
pub(crate) struct FanoutGroup {
    pub id: u16,
    pub mode: FanoutMode,
    pub flags: u16,
    bound_ifindex: u32,
    bound_protocol: u16,
    max_num_members: usize,
    runtime: Arc<FanoutRuntime>,
    members: Vec<FanoutMember>,
}

impl FanoutGroup {
    pub(crate) fn try_new(
        id: u16,
        params: FanoutJoinParams,
        socket: Weak<PacketSocket>,
    ) -> Result<Arc<Self>, SystemError> {
        let rollover = params.mode == FanoutMode::Rollover
            || params.flags & fanout_flag::PACKET_FANOUT_FLAG_ROLLOVER != 0;
        let mut members = Vec::new();
        members
            .try_reserve_exact(1)
            .map_err(|_| SystemError::ENOMEM)?;
        members.push(FanoutMember::try_new(socket, rollover)?);
        Arc::try_new(Self {
            id,
            mode: params.mode,
            flags: params.flags,
            bound_ifindex: params.bound_ifindex,
            bound_protocol: params.bound_protocol,
            max_num_members: if params.max_num_members == 0 {
                LEGACY_FANOUT_MAX_MEMBERS
            } else {
                params.max_num_members as usize
            },
            runtime: FanoutRuntime::try_new()?,
            members,
        })
        .map_err(|_| SystemError::ENOMEM)
    }

    pub(crate) fn matches(&self, params: FanoutJoinParams) -> bool {
        self.mode == params.mode
            && self.flags == params.flags
            && self.bound_ifindex == params.bound_ifindex
            && self.bound_protocol == params.bound_protocol
            && (params.max_num_members == 0
                || self.max_num_members == params.max_num_members as usize)
    }

    pub(crate) fn member_count(&self) -> usize {
        self.members.len()
    }

    pub(crate) fn max_num_members(&self) -> usize {
        self.max_num_members
    }

    pub(crate) fn try_with_member(
        &self,
        socket: Weak<PacketSocket>,
    ) -> Result<Arc<Self>, SystemError> {
        let mut members = Vec::new();
        members
            .try_reserve_exact(self.members.len().saturating_add(1))
            .map_err(|_| SystemError::ENOMEM)?;
        members.extend(self.members.iter().cloned());
        let rollover = self.mode == FanoutMode::Rollover
            || self.flags & fanout_flag::PACKET_FANOUT_FLAG_ROLLOVER != 0;
        members.push(FanoutMember::try_new(socket, rollover)?);
        Arc::try_new(Self {
            id: self.id,
            mode: self.mode,
            flags: self.flags,
            bound_ifindex: self.bound_ifindex,
            bound_protocol: self.bound_protocol,
            max_num_members: self.max_num_members,
            runtime: self.runtime.clone(),
            members,
        })
        .map_err(|_| SystemError::ENOMEM)
    }

    pub(crate) fn try_without_member(
        &self,
        socket: &Weak<PacketSocket>,
    ) -> Result<Arc<Self>, SystemError> {
        let mut members = Vec::new();
        members
            .try_reserve_exact(self.members.len())
            .map_err(|_| SystemError::ENOMEM)?;
        members.extend(self.members.iter().cloned());
        if let Some(index) = members
            .iter()
            .position(|entry| Weak::ptr_eq(&entry.socket, socket))
        {
            // Linux also fills a removed slot with the final member.
            members.swap_remove(index);
        }
        self.try_with_members(members)
    }

    /// Return a compacted replacement when a dead or inactive member is
    /// observed. Healthy groups keep their original Arc and member Vec.
    pub(crate) fn try_without_dead_members(&self) -> Result<Option<Arc<Self>>, SystemError> {
        if self.members.iter().all(|member| {
            member
                .socket
                .upgrade()
                .is_some_and(|socket| socket.is_packet_registry_active())
        }) {
            return Ok(None);
        }
        let mut members = Vec::new();
        members
            .try_reserve_exact(self.members.len())
            .map_err(|_| SystemError::ENOMEM)?;
        members.extend(self.members.iter().cloned());
        let mut index = 0;
        while index < members.len() {
            if !members[index]
                .socket
                .upgrade()
                .is_some_and(|socket| socket.is_packet_registry_active())
            {
                members.swap_remove(index);
            } else {
                index += 1;
            }
        }
        self.try_with_members(members).map(Some)
    }

    fn try_with_members(&self, members: Vec<FanoutMember>) -> Result<Arc<Self>, SystemError> {
        Arc::try_new(Self {
            id: self.id,
            mode: self.mode,
            flags: self.flags,
            bound_ifindex: self.bound_ifindex,
            bound_protocol: self.bound_protocol,
            max_num_members: self.max_num_members,
            runtime: self.runtime.clone(),
            members,
        })
        .map_err(|_| SystemError::ENOMEM)
    }

    /// Deliver to at most one member. Returns `true` when a dead or inactive
    /// member was observed and the writer should compact the topology later.
    pub(crate) fn deliver(
        &self,
        ingress: PacketIngressMetadata,
        frame: &[u8],
        protocol_cache: &mut Option<Option<u16>>,
        flow_hash_cache: &mut Option<u32>,
    ) -> bool {
        if self.members.is_empty()
            || (self.bound_ifindex != 0 && self.bound_ifindex != ingress.ifindex)
            || (self.flags & fanout_flag::PACKET_FANOUT_FLAG_IGNORE_OUTGOING != 0
                && ingress.pkt_type == PacketType::Outgoing)
        {
            return false;
        }

        let needs_protocol =
            self.bound_protocol != eth_protocol::ETH_P_ALL || self.mode == FanoutMode::Hash;
        let protocol = if needs_protocol {
            let Some(protocol) =
                *protocol_cache.get_or_insert_with(|| packet_protocol(frame, ingress.pkt_type))
            else {
                return false;
            };
            if self.bound_protocol != eth_protocol::ETH_P_ALL && self.bound_protocol != protocol {
                return false;
            }
            protocol
        } else {
            eth_protocol::ETH_P_ALL
        };

        let count = self.members.len();
        let base = match self.mode {
            FanoutMode::Hash => {
                let hash = *flow_hash_cache.get_or_insert_with(|| flow_hash(frame, protocol));
                hash as usize % count
            }
            FanoutMode::Lb => {
                self.runtime
                    .rr_counter
                    .fetch_add(1, Ordering::Relaxed)
                    .wrapping_add(1) as usize
                    % count
            }
            FanoutMode::Cpu => crate::smp::core::smp_get_processor_id().data() as usize % count,
            FanoutMode::Rollover => 0,
            FanoutMode::Rnd => random_below(count),
        };

        let rollover = self.mode == FanoutMode::Rollover
            || self.flags & fanout_flag::PACKET_FANOUT_FLAG_ROLLOVER != 0;
        if !rollover {
            return self.deliver_first_live_from(base, ingress, frame);
        }

        let mut stale = false;
        if self.mode != FanoutMode::Rollover {
            match self.members[base].active_socket() {
                Some(socket) if socket.rx_has_room() => {
                    socket.deliver(ingress, frame);
                    return false;
                }
                Some(_) => {}
                None => stale = true,
            }
        }

        let state = self.members[base]
            .rollover
            .as_ref()
            .expect("rollover member state");
        let start = state.cursor.load(Ordering::Relaxed) as usize % count;
        for offset in 0..count {
            let index = (start + offset) % count;
            // Linux's FLAG_ROLLOVER path records the failed primary as
            // `po_skip`; do not reselect it during the fallback scan.
            if self.mode != FanoutMode::Rollover && index == base {
                continue;
            }
            match self.members[index].active_socket() {
                Some(socket) if socket.rx_has_room() => {
                    if index != start {
                        state.cursor.store(index as u32, Ordering::Relaxed);
                    }
                    socket.deliver(ingress, frame);
                    return stale;
                }
                Some(_) => {}
                None => stale = true,
            }
        }

        // Linux returns the initially selected member when every queue is
        // full; the socket receive path then records the single final drop.
        stale | self.deliver_first_live_from(base, ingress, frame)
    }

    fn deliver_first_live_from(
        &self,
        start: usize,
        ingress: PacketIngressMetadata,
        frame: &[u8],
    ) -> bool {
        let mut stale = false;
        for offset in 0..self.members.len() {
            let index = (start + offset) % self.members.len();
            if let Some(socket) = self.members[index].active_socket() {
                socket.deliver(ingress, frame);
                return stale;
            }
            stale = true;
        }
        stale
    }
}

fn hash_seed() -> u32 {
    let current = FLOW_HASH_SEED.load(Ordering::Acquire);
    if current != 0 {
        return current;
    }
    let generated = (crate::arch::rand::rand() as u32) | 1;
    match FLOW_HASH_SEED.compare_exchange(0, generated, Ordering::AcqRel, Ordering::Acquire) {
        Ok(_) => generated,
        Err(installed) => installed,
    }
}

fn random_below(upper: usize) -> usize {
    debug_assert!(upper != 0);
    let max_acceptable = usize::MAX - (usize::MAX % upper + 1) % upper;
    loop {
        let value = crate::arch::rand::rand();
        if value <= max_acceptable {
            return value % upper;
        }
    }
}

/// Linux-compatible symmetric flow-key subset: basic protocol, IP protocol,
/// addresses and ports. Address and port pairs are canonicalized separately.
fn flow_hash(frame: &[u8], protocol: u16) -> u32 {
    let seed = hash_seed();
    let Some((l3_protocol, l3_offset)) = l3_protocol_offset(frame) else {
        return jhash2(&[protocol as u32], seed);
    };
    if l3_protocol == eth_protocol::ETH_P_IP {
        return ipv4_flow_hash(
            frame.get(l3_offset..).unwrap_or_default(),
            l3_protocol,
            seed,
        );
    }
    if l3_protocol == eth_protocol::ETH_P_IPV6 {
        return ipv6_flow_hash(
            frame.get(l3_offset..).unwrap_or_default(),
            l3_protocol,
            seed,
        );
    }
    jhash2(&[l3_protocol as u32], seed)
}

fn l3_protocol_offset(frame: &[u8]) -> Option<(u16, usize)> {
    if frame.len() < 14 {
        return None;
    }
    let mut protocol = u16::from_be_bytes([frame[12], frame[13]]);
    let mut offset = 14usize;
    // Linux's flow dissector walks stacked 802.1Q/802.1ad headers. Keep the
    // same useful QinQ behavior with an explicit bound for hostile frames.
    for _ in 0..8 {
        if protocol != 0x8100 && protocol != 0x88a8 {
            return Some((protocol, offset));
        }
        let header = frame.get(offset..offset.checked_add(4)?)?;
        protocol = u16::from_be_bytes([header[2], header[3]]);
        offset += 4;
    }
    Some((protocol, offset))
}

fn ipv4_flow_hash(l3: &[u8], basic_protocol: u16, seed: u32) -> u32 {
    if l3.len() < 20 || l3[0] >> 4 != 4 {
        return jhash2(&[basic_protocol as u32], seed);
    }
    let ihl = usize::from(l3[0] & 0x0f) * 4;
    if ihl < 20 || l3.len() < ihl {
        return jhash2(&[basic_protocol as u32], seed);
    }
    let mut src = u32::from_be_bytes([l3[12], l3[13], l3[14], l3[15]]);
    let mut dst = u32::from_be_bytes([l3[16], l3[17], l3[18], l3[19]]);
    if dst < src {
        core::mem::swap(&mut src, &mut dst);
    }
    let ip_protocol = l3[9];
    let fragment = u16::from_be_bytes([l3[6], l3[7]]);
    // Linux's symmetric dissector does not request first-fragment parsing:
    // MF or a non-zero offset therefore suppresses L4 ports.
    let ports = if fragment & 0x3fff == 0 {
        canonical_ports(l3.get(ihl..).unwrap_or_default(), ip_protocol)
    } else {
        0
    };
    jhash2(
        &[basic_protocol as u32, ip_protocol as u32, src, dst, ports],
        seed,
    )
}

fn ipv6_flow_hash(l3: &[u8], basic_protocol: u16, seed: u32) -> u32 {
    if l3.len() < 40 || l3[0] >> 4 != 6 {
        return jhash2(&[basic_protocol as u32], seed);
    }
    let mut src = [0u32; 4];
    let mut dst = [0u32; 4];
    for index in 0..4 {
        let src_off = 8 + index * 4;
        let dst_off = 24 + index * 4;
        src[index] = u32::from_be_bytes(l3[src_off..src_off + 4].try_into().unwrap());
        dst[index] = u32::from_be_bytes(l3[dst_off..dst_off + 4].try_into().unwrap());
    }
    if compare_words(&dst, &src) == CmpOrdering::Less {
        core::mem::swap(&mut src, &mut dst);
    }
    let (ip_protocol, transport_offset, has_ports) = ipv6_transport(l3);
    let ports = if has_ports {
        canonical_ports(l3.get(transport_offset..).unwrap_or_default(), ip_protocol)
    } else {
        0
    };
    let words = [
        basic_protocol as u32,
        ip_protocol as u32,
        src[0],
        src[1],
        src[2],
        src[3],
        dst[0],
        dst[1],
        dst[2],
        dst[3],
        ports,
    ];
    jhash2(&words, seed)
}

fn ipv6_transport(l3: &[u8]) -> (u8, usize, bool) {
    let mut next = l3[6];
    let mut offset = 40usize;
    for _ in 0..8 {
        match next {
            0 | 43 | 60 => {
                let Some(header) = l3.get(offset..offset.saturating_add(2)) else {
                    return (next, offset, false);
                };
                next = header[0];
                offset = match offset.checked_add((usize::from(header[1]) + 1) * 8) {
                    Some(value) if value <= l3.len() => value,
                    _ => return (next, offset, false),
                };
            }
            44 => {
                let Some(header) = l3.get(offset..offset.saturating_add(8)) else {
                    return (next, offset, false);
                };
                next = header[0];
                offset += 8;
                // Every IPv6 Fragment Header, including an atomic fragment,
                // sets FLOW_DIS_IS_FRAGMENT and suppresses port extraction.
                return (next, offset, false);
            }
            _ => return (next, offset, true),
        }
    }
    (next, offset, false)
}

fn canonical_ports(l4: &[u8], protocol: u8) -> u32 {
    if (protocol != IPPROTO_TCP && protocol != IPPROTO_UDP) || l4.len() < 4 {
        return 0;
    }
    let mut src = u16::from_be_bytes([l4[0], l4[1]]);
    let mut dst = u16::from_be_bytes([l4[2], l4[3]]);
    if dst < src {
        core::mem::swap(&mut src, &mut dst);
    }
    (u32::from(src) << 16) | u32::from(dst)
}

fn compare_words(left: &[u32; 4], right: &[u32; 4]) -> CmpOrdering {
    for index in 0..4 {
        match left[index].cmp(&right[index]) {
            CmpOrdering::Equal => {}
            ordering => return ordering,
        }
    }
    CmpOrdering::Equal
}

impl PacketSocket {
    pub(crate) fn join_fanout(&self, raw: u32, max_num_members: u32) -> Result<(), SystemError> {
        let id_req = raw as u16;
        let type_flags = raw >> 16;
        let mode = FanoutMode::from_raw(type_flags & 0xff)?;
        let mut flags = (type_flags & !0xff) as u16;

        const ALLOWED_FLAGS: u16 = fanout_flag::PACKET_FANOUT_FLAG_ROLLOVER
            | fanout_flag::PACKET_FANOUT_FLAG_UNIQUEID
            | fanout_flag::PACKET_FANOUT_FLAG_IGNORE_OUTGOING;
        if flags & !ALLOWED_FLAGS != 0
            || (mode == FanoutMode::Rollover
                && flags & fanout_flag::PACKET_FANOUT_FLAG_ROLLOVER != 0)
        {
            return Err(SystemError::EINVAL);
        }
        if max_num_members as usize > FANOUT_MAX_MEMBERS {
            return Err(SystemError::EINVAL);
        }

        let unique = flags & fanout_flag::PACKET_FANOUT_FLAG_UNIQUEID != 0;
        if unique && id_req != 0 {
            return Err(SystemError::EINVAL);
        }
        flags &= !fanout_flag::PACKET_FANOUT_FLAG_UNIQUEID;

        let _guard = self.bind_lock.lock();
        if self.has_fanout_group() {
            return Err(SystemError::EALREADY);
        }
        let (bound_ifindex, bound_protocol) = self.binding.load();
        if bound_protocol == 0 {
            return Err(SystemError::EINVAL);
        }
        let socket = self.self_ref.upgrade().ok_or(SystemError::EINVAL)?;
        self.netns.fanout_group_join(
            &socket,
            FanoutJoinParams {
                id_req,
                unique,
                mode,
                flags,
                bound_ifindex,
                bound_protocol,
                max_num_members,
            },
        )
    }

    pub(crate) fn has_fanout_group(&self) -> bool {
        self.fanout_membership.load(Ordering::Acquire) >> 32 != 0
    }

    pub(crate) fn set_fanout_membership(&self, value: u32) {
        self.fanout_membership
            .store((1u64 << 32) | u64::from(value), Ordering::Release);
    }

    pub(crate) fn clear_fanout_membership(&self) {
        self.fanout_membership.store(0, Ordering::Release);
    }

    pub(crate) fn fanout_getsockopt_value(&self) -> Option<u32> {
        let membership = self.fanout_membership.load(Ordering::Acquire);
        (membership >> 32 != 0).then_some(membership as u32)
    }

    pub(crate) fn fanout_group_id(&self) -> Option<u16> {
        self.fanout_getsockopt_value().map(|value| value as u16)
    }

    pub(super) fn rx_has_room(&self) -> bool {
        self.rx_buffer_bytes.load(Ordering::Acquire)
            < self.recv_buffer_bytes.load(Ordering::Relaxed)
    }
}

pub(crate) fn membership_value(group: &FanoutGroup) -> u32 {
    u32::from(group.id) | ((group.mode.as_raw() | u32::from(group.flags)) << 16)
}
