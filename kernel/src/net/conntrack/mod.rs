//! Per-network-namespace connection tracking runtime.
//!
//! A packet owns its unconfirmed candidate. Nothing is published until the
//! packet has passed the final INPUT or POSTROUTING hook. Confirmation checks
//! both directions again under one lock, because NAT's earlier port choice is
//! only provisional. This state is deliberately separate from nft rule COW.

use crate::{
    libs::spinlock::SpinLock,
    time::{Duration, Instant},
};
use alloc::sync::Arc;
use core::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use hashbrown::HashMap;
use smoltcp::wire::IpVersion;

mod control;
mod nat;
mod nat_v6;
mod packet;
mod packet_v6;
mod tcp;
pub(crate) use control::{CtFilter, CtRecord, CtTupleFilter};
pub(crate) use nat::{
    rewrite_ipv4_related_icmp, rewrite_ipv4_tuple, NatManipSide, NatRewriteError,
};
pub(crate) use nat_v6::{rewrite_ipv6_related_icmp, rewrite_ipv6_tuple};
pub(crate) use packet::{
    parse_ipv4_conntrack, parse_ipv4_conntrack_with_mode, CtChecksumMode, ParsedCtPacket,
};
pub(crate) use packet_v6::{parse_ipv6_conntrack, parse_ipv6_conntrack_with_mode};
#[cfg(test)]
use tcp::TcpOptions;
pub(crate) use tcp::TcpSegment;
use tcp::{TcpTracker, TcpVerdict};

const UDP_UNREPLIED: Duration = Duration::from_secs(30);
const UDP_REPLIED: Duration = Duration::from_secs(120);
const ICMP_TIMEOUT: Duration = Duration::from_secs(30);
const GENERIC_TIMEOUT: Duration = Duration::from_secs(600);
// The table is intentionally bounded while expiry uses a single hash-table
// pass under its lock. A larger table needs a separately designed, incremental
// expiry index rather than an unbounded packet/poller critical section.
const MAX_FLOWS: usize = 4096;
const GC_BATCH: usize = 64;
// Port choice is provisional. Keep search bounded on the packet path;
// confirmation remains the arbiter when candidates race.
// Linux caps each softirq search round at 128 and halves later rounds. Our
// tuple/time-salted search has the same 128+64+32+16+8 upper bound;
// unlike Linux it fails closed when those probes all collide rather than
// proposing a duplicate for confirm to reject. This is collision spreading,
// not a cryptographically unpredictable ephemeral-port allocation policy:
// the kernel's secure RNG may block on virtio refill and is not called here.
const NAT_PORT_ATTEMPTS: [u32; 5] = [128, 64, 32, 16, 8];

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum CtL4 {
    Tcp {
        src_port: u16,
        dst_port: u16,
    },
    Udp {
        src_port: u16,
        dst_port: u16,
    },
    Icmp {
        identifier: u16,
        kind: u8,
        code: u8,
    },
    Icmpv6 {
        identifier: u16,
        kind: u8,
        code: u8,
    },
    /// Linux's generic tracker has no transport-port discriminator. Its
    /// protocol number is still part of the conntrack tuple.
    Generic {
        protocol: u8,
    },
}

impl CtL4 {
    fn reverse(self) -> Option<Self> {
        Some(match self {
            Self::Tcp { src_port, dst_port } => Self::Tcp {
                src_port: dst_port,
                dst_port: src_port,
            },
            Self::Udp { src_port, dst_port } => Self::Udp {
                src_port: dst_port,
                dst_port: src_port,
            },
            Self::Icmp {
                identifier,
                kind,
                code,
            } => Self::Icmp {
                identifier,
                kind: match kind {
                    8 => 0,
                    0 => 8,
                    13 => 14,
                    14 => 13,
                    15 => 16,
                    16 => 15,
                    17 => 18,
                    18 => 17,
                    _ => return None,
                },
                code,
            },
            Self::Icmpv6 {
                identifier,
                kind,
                code,
            } => Self::Icmpv6 {
                identifier,
                kind: match kind {
                    128 => 129,
                    129 => 128,
                    139 => 140,
                    140 => 139,
                    _ => return None,
                },
                code,
            },
            Self::Generic { protocol } => Self::Generic { protocol },
        })
    }
}

/// A family-tagged address prevents IPv4 and IPv6 tuples with identical low
/// bytes from colliding. NAT64 is not implied by sharing the flow index.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum CtAddress {
    V4([u8; 4]),
    V6([u8; 16]),
}

impl From<[u8; 4]> for CtAddress {
    fn from(value: [u8; 4]) -> Self {
        Self::V4(value)
    }
}

impl From<[u8; 16]> for CtAddress {
    fn from(value: [u8; 16]) -> Self {
        Self::V6(value)
    }
}

/// Addresses and ports are in host representation. The packet parser must
/// validate IP/L4 lengths and checksums before constructing this key.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct CtTuple {
    pub(crate) src: CtAddress,
    pub(crate) dst: CtAddress,
    pub(crate) l4: CtL4,
}

impl CtTuple {
    fn same_family(self) -> bool {
        matches!(
            (self.src, self.dst),
            (CtAddress::V4(_), CtAddress::V4(_)) | (CtAddress::V6(_), CtAddress::V6(_))
        )
    }

    pub(crate) fn reverse(self) -> Option<Self> {
        Some(Self {
            src: self.dst,
            dst: self.src,
            l4: self.l4.reverse()?,
        })
    }

    fn destination_from(mut self, other: Self) -> Self {
        self.dst = other.dst;
        self.l4 = match (self.l4, other.l4) {
            (CtL4::Tcp { src_port, .. }, CtL4::Tcp { dst_port, .. }) => {
                CtL4::Tcp { src_port, dst_port }
            }
            (CtL4::Udp { src_port, .. }, CtL4::Udp { dst_port, .. }) => {
                CtL4::Udp { src_port, dst_port }
            }
            (old, _) => old,
        };
        self
    }

    fn source_from(mut self, other: Self) -> Self {
        self.src = other.src;
        self.l4 = match (self.l4, other.l4) {
            (CtL4::Tcp { dst_port, .. }, CtL4::Tcp { src_port, .. }) => {
                CtL4::Tcp { src_port, dst_port }
            }
            (CtL4::Udp { dst_port, .. }, CtL4::Udp { src_port, .. }) => {
                CtL4::Udp { src_port, dst_port }
            }
            (CtL4::Icmp { kind, code, .. }, CtL4::Icmp { identifier, .. }) => CtL4::Icmp {
                identifier,
                kind,
                code,
            },
            (CtL4::Icmpv6 { kind, code, .. }, CtL4::Icmpv6 { identifier, .. }) => CtL4::Icmpv6 {
                identifier,
                kind,
                code,
            },
            (old, _) => old,
        };
        self
    }
}

/// The outer NAT hook rewrites exactly one side. The second side's input is
/// the first side's output; a reply enters with the inverse translated tuple.
fn nat_rewrite_plan(
    original: CtTuple,
    translated: CtTuple,
    direction: CtDirection,
    side: NatManipSide,
) -> CtNatRewrite {
    let forward_middle = original.destination_from(translated);
    let reply_start = translated.reverse().expect("validated translation");
    let reply_end = original.reverse().expect("validated original");
    let mut reply_middle = reply_start.destination_from(reply_end);
    if matches!(reply_middle.l4, CtL4::Icmp { .. } | CtL4::Icmpv6 { .. }) {
        // Echo identifiers belong to the tuple's source field even when the
        // reply's destination hook reverses the original source mapping.
        reply_middle.l4 = reply_end.l4;
    }
    let (from, to) = match (direction, side) {
        (CtDirection::Original, NatManipSide::Destination) => (original, forward_middle),
        (CtDirection::Original, NatManipSide::Source) => (forward_middle, translated),
        (CtDirection::Reply, NatManipSide::Destination) => (reply_start, reply_middle),
        (CtDirection::Reply, NatManipSide::Source) => (reply_middle, reply_end),
    };
    CtNatRewrite { from, to }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CtPacketKind {
    Tcp(TcpSegment),
    Udp,
    IcmpQuery,
    Generic,
}

impl CtPacketKind {
    fn matches(self, tuple: CtTuple) -> bool {
        matches!(
            (self, tuple.l4),
            (Self::Tcp(_), CtL4::Tcp { .. })
                | (Self::Udp, CtL4::Udp { .. })
                | (Self::IcmpQuery, CtL4::Icmp { .. })
                | (Self::IcmpQuery, CtL4::Icmpv6 { .. })
                | (Self::Generic, CtL4::Generic { .. })
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CtDirection {
    Original,
    Reply,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CtPacketState {
    New,
    Established,
    Related,
    Invalid,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CtError {
    Inactive,
    Invalid,
    Collision,
    Full,
    NoMemory,
}

/// Validated L4 port/echo-identifier interval. The ruleset compiler owns UAPI
/// flags and register validation; this type only expresses a usable range.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CtNatPortRange {
    first: u16,
    last: u16,
}

impl CtNatPortRange {
    pub(crate) fn new(first: u16, last: u16) -> Result<Self, CtError> {
        if first > last {
            return Err(CtError::Invalid);
        }
        Ok(Self { first, last })
    }
}

/// A single destination/source address or a port-only mapping. Address ranges
/// and unsupported NAT flags must be rejected by the caller before this API.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CtNatRequest {
    address: Option<CtAddress>,
    ports: Option<CtNatPortRange>,
    /// Linux records the MASQUERADE egress ifindex even when it selects an
    /// address configured on another interface in the same L3 domain.
    masquerade_ifindex: Option<u32>,
}

impl CtNatRequest {
    pub(crate) fn new(
        address: Option<CtAddress>,
        ports: Option<CtNatPortRange>,
    ) -> Result<Self, CtError> {
        if address.is_none() && ports.is_none() {
            return Err(CtError::Invalid);
        }
        Ok(Self {
            address,
            ports,
            masquerade_ifindex: None,
        })
    }

    pub(crate) fn masquerade(
        address: CtAddress,
        ifindex: u32,
        ports: Option<CtNatPortRange>,
    ) -> Result<Self, CtError> {
        if ifindex == 0 {
            return Err(CtError::Invalid);
        }
        Ok(Self {
            address: Some(address),
            ports,
            masquerade_ifindex: Some(ifindex),
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CtNatRewrite {
    pub(crate) from: CtTuple,
    pub(crate) to: CtTuple,
}

impl CtNatRewrite {
    /// Packet ownership and hook ordering remain with ingress/output. This
    /// method validates the exact incoming tuple before touching its bytes.
    pub(crate) fn apply(
        self,
        packet: &mut [u8],
        mode: CtChecksumMode,
    ) -> Result<(), NatRewriteError> {
        match (self.from.src, self.to.src) {
            (CtAddress::V4(_), CtAddress::V4(_)) => {
                rewrite_ipv4_tuple(packet, self.from, self.to, mode)
            }
            (CtAddress::V6(_), CtAddress::V6(_)) => {
                rewrite_ipv6_tuple(packet, self.from, self.to, mode)
            }
            _ => Err(NatRewriteError::Unsupported),
        }
    }
}

fn nat_port(tuple: CtTuple, side: NatManipSide) -> Option<u16> {
    match (tuple.l4, side) {
        (CtL4::Tcp { src_port, .. } | CtL4::Udp { src_port, .. }, NatManipSide::Source) => {
            Some(src_port)
        }
        (CtL4::Tcp { dst_port, .. } | CtL4::Udp { dst_port, .. }, NatManipSide::Destination) => {
            Some(dst_port)
        }
        (CtL4::Icmp { identifier, .. }, NatManipSide::Source)
        | (
            CtL4::Icmpv6 {
                identifier,
                kind: 128 | 129,
                ..
            },
            NatManipSide::Source,
        ) => Some(identifier),
        _ => None,
    }
}

fn with_nat_port(mut tuple: CtTuple, side: NatManipSide, port: u16) -> CtTuple {
    tuple.l4 = match (tuple.l4, side) {
        (CtL4::Tcp { dst_port, .. }, NatManipSide::Source) => CtL4::Tcp {
            src_port: port,
            dst_port,
        },
        (CtL4::Tcp { src_port, .. }, NatManipSide::Destination) => CtL4::Tcp {
            src_port,
            dst_port: port,
        },
        (CtL4::Udp { dst_port, .. }, NatManipSide::Source) => CtL4::Udp {
            src_port: port,
            dst_port,
        },
        (CtL4::Udp { src_port, .. }, NatManipSide::Destination) => CtL4::Udp {
            src_port,
            dst_port: port,
        },
        (CtL4::Icmp { kind, code, .. }, NatManipSide::Source) => CtL4::Icmp {
            identifier: port,
            kind,
            code,
        },
        (CtL4::Icmpv6 { kind, code, .. }, NatManipSide::Source) => CtL4::Icmpv6 {
            identifier: port,
            kind,
            code,
        },
        (old, _) => old,
    };
    tuple
}

/// Linux preserves the traditional source-port classes when it has to find
/// an implicit source mapping. Without an explicit range, destination NAT
/// never changes the transport port to escape a collision.
fn implicit_port_range(port: u16) -> CtNatPortRange {
    let (first, last) = match port {
        0..=511 => (1, 511),
        512..=1023 => (600, 1023),
        _ => (1024, u16::MAX),
    };
    CtNatPortRange { first, last }
}

fn nat_probe_seed(tuple: CtTuple, now: Instant) -> u32 {
    let mut hash = (now.total_micros() as u32) ^ 0x811c9dc5;
    let mut feed = |bytes: &[u8]| {
        for &byte in bytes {
            hash = (hash ^ u32::from(byte)).wrapping_mul(0x01000193);
        }
    };
    match tuple.src {
        CtAddress::V4(bytes) => feed(&bytes),
        CtAddress::V6(bytes) => feed(&bytes),
    }
    match tuple.dst {
        CtAddress::V4(bytes) => feed(&bytes),
        CtAddress::V6(bytes) => feed(&bytes),
    }
    match tuple.l4 {
        CtL4::Tcp { src_port, dst_port } | CtL4::Udp { src_port, dst_port } => {
            feed(&src_port.to_be_bytes());
            feed(&dst_port.to_be_bytes());
        }
        CtL4::Icmp { identifier, .. } | CtL4::Icmpv6 { identifier, .. } => {
            feed(&identifier.to_be_bytes());
        }
        CtL4::Generic { protocol } => feed(&[protocol]),
    }
    hash
}

enum FlowDecision {
    Matched { state: CtPacketState, destroy: bool },
    Invalid,
    Repeat,
}

#[derive(Debug)]
struct FlowRuntime {
    seen_reply: bool,
    udp_stream_after: Instant,
    udp_assured: bool,
    tcp: Option<TcpTracker>,
    expires_at: Instant,
}

impl FlowRuntime {
    fn new(kind: CtPacketKind, now: Instant) -> Self {
        let (tcp, timeout) = match kind {
            CtPacketKind::Tcp(segment) => {
                let (tracker, timeout) = TcpTracker::new(segment).expect("validated TCP candidate");
                (Some(tracker), timeout)
            }
            CtPacketKind::Udp => (None, UDP_UNREPLIED),
            CtPacketKind::IcmpQuery => (None, ICMP_TIMEOUT),
            CtPacketKind::Generic => (None, GENERIC_TIMEOUT),
        };
        Self {
            seen_reply: false,
            udp_stream_after: now + Duration::from_secs(2),
            udp_assured: false,
            tcp,
            expires_at: now + timeout,
        }
    }

    fn observe(
        &mut self,
        kind: CtPacketKind,
        direction: CtDirection,
        now: Instant,
    ) -> FlowDecision {
        if let (Some(tracker), CtPacketKind::Tcp(segment)) = (self.tcp.as_mut(), kind) {
            let direction_index = usize::from(direction == CtDirection::Reply);
            return match tracker.observe(segment, direction_index) {
                TcpVerdict::Accepted { timeout, destroy } => {
                    if let Some(timeout) = timeout {
                        self.expires_at = now + timeout;
                    }
                    FlowDecision::Matched {
                        state: if tracker.seen_reply() {
                            CtPacketState::Established
                        } else {
                            CtPacketState::New
                        },
                        destroy,
                    }
                }
                TcpVerdict::Ignored => FlowDecision::Matched {
                    state: if tracker.seen_reply() {
                        CtPacketState::Established
                    } else {
                        CtPacketState::New
                    },
                    destroy: false,
                },
                TcpVerdict::Invalid => FlowDecision::Invalid,
                TcpVerdict::Repeat => FlowDecision::Repeat,
            };
        }
        let timeout = match kind {
            CtPacketKind::Udp if self.seen_reply && now > self.udp_stream_after => {
                self.udp_assured = true;
                UDP_REPLIED
            }
            CtPacketKind::Udp => UDP_UNREPLIED,
            CtPacketKind::IcmpQuery => ICMP_TIMEOUT,
            CtPacketKind::Generic => GENERIC_TIMEOUT,
            CtPacketKind::Tcp(_) => return FlowDecision::Invalid,
        };
        if direction == CtDirection::Reply {
            self.seen_reply = true;
        }
        self.expires_at = now + timeout;
        FlowDecision::Matched {
            state: if self.seen_reply {
                CtPacketState::Established
            } else {
                CtPacketState::New
            },
            destroy: false,
        }
    }
}

#[derive(Debug)]
pub(crate) struct CtFlow {
    /// Publication sequence, assigned under the table lock. Not a pointer or
    /// a second tuple index. Its low 32 bits are the opaque ctnetlink ID.
    serial: AtomicU64,
    nat_done: u32,
    original: CtTuple,
    /// Tuple on the wire after both destination and source NAT.
    translated: CtTuple,
    reply: CtTuple,
    masquerade_ifindex: Option<u32>,
    runtime: SpinLock<FlowRuntime>,
}

impl CtFlow {
    pub(crate) fn original(&self) -> CtTuple {
        self.original
    }
    pub(crate) fn translated(&self) -> CtTuple {
        self.translated
    }
    pub(crate) fn reply(&self) -> CtTuple {
        self.reply
    }

    pub(crate) fn masquerade_ifindex(&self) -> Option<u32> {
        self.masquerade_ifindex
    }

    pub(crate) fn nat_rewrite(&self, direction: CtDirection, side: NatManipSide) -> CtNatRewrite {
        nat_rewrite_plan(self.original, self.translated, direction, side)
    }
}

#[derive(Clone, Debug)]
pub(crate) struct CtCandidate {
    original: CtTuple,
    translated: CtTuple,
    kind: CtPacketKind,
    destination_initialized: bool,
    source_initialized: bool,
    masquerade_ifindex: Option<u32>,
    masquerade_generation: Option<u64>,
}

impl CtCandidate {
    pub(crate) fn new(original: CtTuple, kind: CtPacketKind) -> Result<Self, CtError> {
        if !original.same_family()
            || !kind.matches(original)
            || matches!(
                (original.src, original.l4),
                (CtAddress::V4(_), CtL4::Icmpv6 { .. }) | (CtAddress::V6(_), CtL4::Icmp { .. })
            )
            || original.reverse().is_none()
            || matches!(
                original.l4,
                CtL4::Icmp {
                    kind: 0 | 14 | 16 | 18,
                    ..
                }
            )
            || matches!(
                original.l4,
                CtL4::Icmpv6 {
                    kind: 129 | 140,
                    ..
                }
            )
            || matches!(kind, CtPacketKind::Tcp(segment) if TcpTracker::new(segment).is_none())
        {
            return Err(CtError::Invalid);
        }
        Ok(Self {
            original,
            translated: original,
            kind,
            destination_initialized: false,
            source_initialized: false,
            masquerade_ifindex: None,
            masquerade_generation: None,
        })
    }

    pub(crate) fn original(&self) -> CtTuple {
        self.original
    }
    pub(crate) fn translated(&self) -> CtTuple {
        self.translated
    }

    pub(crate) fn nat_initialized(&self, side: NatManipSide) -> bool {
        match side {
            NatManipSide::Destination => self.destination_initialized,
            NatManipSide::Source => self.source_initialized,
        }
    }

    /// No rule selected a mapping at this outer hook. This is still an
    /// initialized side: a looped-back first packet must not rerun NAT rules.
    pub(crate) fn initialize_null_binding(&mut self, side: NatManipSide) -> Result<(), CtError> {
        if self.nat_initialized(side) {
            return Err(CtError::Invalid);
        }
        self.mark_nat_initialized(side);
        Ok(())
    }

    fn mark_nat_initialized(&mut self, side: NatManipSide) {
        match side {
            NatManipSide::Destination => self.destination_initialized = true,
            NatManipSide::Source => self.source_initialized = true,
        }
    }

    pub(crate) fn nat_rewrite(&self, side: NatManipSide) -> CtNatRewrite {
        nat_rewrite_plan(self.original, self.translated, CtDirection::Original, side)
    }

    /// Called only after a NAT expression has selected a complete mapping.
    /// The caller must then rewrite the packet and repair its checksums.
    fn set_translated(&mut self, translated: CtTuple) -> Result<(), CtError> {
        if core::mem::discriminant(&self.original.l4) != core::mem::discriminant(&translated.l4)
            || !translated.same_family()
            || core::mem::discriminant(&self.original.src)
                != core::mem::discriminant(&translated.src)
            || translated.reverse().is_none()
        {
            return Err(CtError::Invalid);
        }
        self.translated = translated;
        Ok(())
    }

    pub(crate) fn proposed_reply(&self) -> CtTuple {
        self.translated.reverse().expect("validated candidate")
    }

    /// An ICMP error generated before POST_ROUTING still belongs to this
    /// private first packet. Give the error a temporary RELATED identity
    /// without publishing the rejected original flow in the conntrack table.
    fn related_error_match(&self, now: Instant) -> Result<CtMatch, CtError> {
        let flow = Arc::try_new(CtFlow {
            serial: AtomicU64::new(0),
            nat_done: 0,
            original: self.original,
            translated: self.translated,
            reply: self.proposed_reply(),
            masquerade_ifindex: self.masquerade_ifindex,
            runtime: SpinLock::new(FlowRuntime::new(self.kind, now)),
        })
        .map_err(|_| CtError::NoMemory)?;
        Ok(CtMatch {
            flow,
            direction: CtDirection::Reply,
            state: CtPacketState::Related,
        })
    }
}

#[derive(Clone, Debug)]
pub(crate) struct CtMatch {
    pub(crate) flow: Arc<CtFlow>,
    pub(crate) direction: CtDirection,
    pub(crate) state: CtPacketState,
}

impl CtMatch {
    pub(crate) fn nat_rewrite(&self, side: NatManipSide) -> Option<CtNatRewrite> {
        (self.state != CtPacketState::Related && self.state != CtPacketState::Invalid)
            .then(|| self.flow.nat_rewrite(self.direction, side))
    }

    /// An ICMP error quotes the opposite direction. At the destination outer
    /// hook its quoted source changes; at the source hook its quoted
    /// destination changes. `quoted_at_hook` accounts for any prior rewrite.
    pub(crate) fn related_quote_rewrite(
        &self,
        side: NatManipSide,
        quoted_at_hook: CtTuple,
    ) -> Option<CtNatRewrite> {
        if self.state != CtPacketState::Related {
            return None;
        }
        let original = self.flow.original();
        let translated = self.flow.translated();
        let (start, target) = match self.direction {
            CtDirection::Reply => (translated, original),
            CtDirection::Original => (original.reverse()?, translated.reverse()?),
        };
        let middle = start.source_from(target);
        let (from, to) = match side {
            NatManipSide::Destination => (start, middle),
            NatManipSide::Source => (middle, middle.destination_from(target)),
        };
        (quoted_at_hook == from).then_some(CtNatRewrite { from, to })
    }

    /// The trigger can be between PRE_ROUTING and POST_ROUTING. Its complete
    /// tuple, captured before the ICMP quote is shortened, is authoritative
    /// for this one kernel-generated error. Network-received ICMP errors keep
    /// using `related_quote_rewrite` and strict tuple parsing.
    fn generated_related_plan(
        &self,
        quote_start: CtTuple,
        side: NatManipSide,
    ) -> Result<CtNatRewrite, NatRewriteError> {
        let target = match self.direction {
            CtDirection::Reply => self.flow.original(),
            CtDirection::Original => self
                .flow
                .translated()
                .reverse()
                .ok_or(NatRewriteError::Unsupported)?,
        };
        let middle = quote_start.source_from(target);
        let (from, to) = match side {
            NatManipSide::Destination => (quote_start, middle),
            NatManipSide::Source => (middle, target),
        };
        Ok(CtNatRewrite { from, to })
    }

    fn rewrite_generated_related_icmp(
        &self,
        packet: &mut [u8],
        quote_start: CtTuple,
        side: NatManipSide,
        mode: CtChecksumMode,
    ) -> Result<(), NatRewriteError> {
        let plan = self.generated_related_plan(quote_start, side)?;
        match plan.from.src {
            CtAddress::V4(_) => {
                nat::rewrite_ipv4_generated_related_icmp(packet, plan.from, plan.to, side, mode)
            }
            CtAddress::V6(_) => {
                nat_v6::rewrite_ipv6_generated_related_icmp(packet, plan.from, plan.to, side, mode)
            }
        }
    }

    /// RELATED errors need quote and outer-header manipulation together.
    /// The existing family helpers validate the quote and fix all checksums.
    pub(crate) fn rewrite_related_icmp(
        &self,
        packet: &mut [u8],
        quoted_at_hook: CtTuple,
        side: NatManipSide,
        mode: CtChecksumMode,
    ) -> Result<(), NatRewriteError> {
        let plan = self
            .related_quote_rewrite(side, quoted_at_hook)
            .ok_or(NatRewriteError::TupleMismatch)?;
        let any_nat = self.flow.original != self.flow.translated;
        match plan.from.src {
            CtAddress::V4(_) => {
                if any_nat && nat::is_ipv4_related_redirect(packet, mode) {
                    return Err(NatRewriteError::Unsupported);
                }
                rewrite_ipv4_related_icmp(packet, plan.from, plan.to, side, mode)
            }
            CtAddress::V6(_) => {
                if any_nat && nat_v6::is_ipv6_related_redirect(packet, mode) {
                    return Err(NatRewriteError::Unsupported);
                }
                rewrite_ipv6_related_icmp(packet, plan.from, plan.to, side, mode)
            }
        }
    }
}

/// Connection identity belongs to one packet, not to a poll or a socket.
/// A new flow remains private until the last INPUT/POSTROUTING hook accepts
/// the packet. In particular, a filter DROP must not publish a candidate.
#[derive(Clone, Debug)]
pub(crate) enum CtPacketContext {
    Untracked,
    Invalid,
    Matched(CtMatch),
    /// Only locally generated ICMP errors carry the full triggering tuple.
    /// Their bounded quote may be shorter than a transport header, so NAT
    /// must not try to reconstruct this tuple from the shortened bytes.
    GeneratedRelated {
        found: CtMatch,
        quote_start: CtTuple,
    },
    Candidate(CtCandidate),
}

impl CtPacketContext {
    /// Attach the triggering packet's identity to an internally generated
    /// ICMP error. The error travels opposite to the triggering direction.
    pub(crate) fn related_error_context(
        &self,
        now: Instant,
        trigger: &[u8],
    ) -> Result<Option<Self>, CtError> {
        if matches!(self, Self::Untracked | Self::Invalid) {
            return Ok(None);
        }
        let parsed = match IpVersion::of_packet(trigger) {
            Ok(IpVersion::Ipv4) => parse_ipv4_conntrack_with_mode(trigger, CtChecksumMode::Skip),
            Ok(IpVersion::Ipv6) => parse_ipv6_conntrack_with_mode(trigger, CtChecksumMode::Skip),
            _ => return Err(CtError::Invalid),
        };
        let ParsedCtPacket::Flow {
            tuple: quote_start, ..
        } = parsed
        else {
            return Err(CtError::Invalid);
        };
        match self {
            Self::Candidate(candidate) => candidate
                .related_error_match(now)
                .map(|found| Some(Self::GeneratedRelated { found, quote_start })),
            Self::Matched(matched) => Ok(Some(Self::GeneratedRelated {
                found: CtMatch {
                    flow: matched.flow.clone(),
                    direction: match matched.direction {
                        CtDirection::Original => CtDirection::Reply,
                        CtDirection::Reply => CtDirection::Original,
                    },
                    state: CtPacketState::Related,
                },
                quote_start,
            })),
            Self::GeneratedRelated { .. } => Err(CtError::Invalid),
            Self::Untracked | Self::Invalid => Ok(None),
        }
    }
    pub(crate) fn state(&self) -> Option<CtPacketState> {
        match self {
            Self::Untracked => None,
            Self::Invalid => Some(CtPacketState::Invalid),
            Self::Matched(found) => Some(found.state),
            Self::GeneratedRelated { .. } => Some(CtPacketState::Related),
            Self::Candidate(_) => Some(CtPacketState::New),
        }
    }

    pub(crate) fn into_candidate(self) -> Option<CtCandidate> {
        match self {
            Self::Candidate(candidate) => Some(candidate),
            _ => None,
        }
    }

    /// Run a fixed outer NAT hook for a confirmed packet. NAT rules must not
    /// be evaluated again for an existing flow. A candidate instead receives
    /// its first binding from the NAT chain and is handled by its caller.
    ///
    /// Returns `true` when the packet already had a conntrack identity that
    /// excludes first-packet rule selection. An untracked or invalid packet
    /// likewise cannot create a NAT binding at this hook.
    pub(crate) fn rewrite_confirmed_nat(
        &self,
        packet: &mut [u8],
        side: NatManipSide,
        mode: CtChecksumMode,
    ) -> Result<bool, NatRewriteError> {
        match self {
            Self::Candidate(_) => Ok(false),
            Self::GeneratedRelated { found, quote_start } => {
                found.rewrite_generated_related_icmp(packet, *quote_start, side, mode)?;
                Ok(true)
            }
            Self::Matched(found) if found.state == CtPacketState::Related => {
                let parsed = match IpVersion::of_packet(packet) {
                    Ok(IpVersion::Ipv4) => parse_ipv4_conntrack_with_mode(packet, mode),
                    Ok(IpVersion::Ipv6) => parse_ipv6_conntrack_with_mode(packet, mode),
                    _ => return Err(NatRewriteError::InvalidPacket),
                };
                let ParsedCtPacket::Related { quoted, .. } = parsed else {
                    return Err(NatRewriteError::InvalidPacket);
                };
                found.rewrite_related_icmp(packet, quoted, side, mode)?;
                Ok(true)
            }
            Self::Matched(found) => {
                let plan = found
                    .nat_rewrite(side)
                    .ok_or(NatRewriteError::InvalidPacket)?;
                plan.apply(packet, mode)?;
                Ok(true)
            }
            Self::Untracked | Self::Invalid => Ok(true),
        }
    }
}

#[derive(Debug)]
pub(crate) enum CtConfirm {
    Inserted(Arc<CtFlow>),
    Reused(Arc<CtFlow>),
}

#[derive(Debug)]
struct CtTable {
    last_serial: u64,
    original: HashMap<CtTuple, Arc<CtFlow>>,
    reply: HashMap<CtTuple, Arc<CtFlow>>,
    max_flows: usize,
    next_expiry: Option<Instant>,
    masquerade_generation: u64,
}

impl CtTable {
    fn nat_tuple_available(&self, original: CtTuple, translated: CtTuple, now: Instant) -> bool {
        let Some(reply) = translated.reverse() else {
            return false;
        };
        [original, reply].into_iter().all(|key| {
            self.original
                .get(&key)
                .is_none_or(|flow| flow.runtime.lock().expires_at <= now)
                && self
                    .reply
                    .get(&key)
                    .is_none_or(|flow| flow.runtime.lock().expires_at <= now)
        })
    }

    fn remove_flow(&mut self, flow: Arc<CtFlow>) -> Arc<CtFlow> {
        self.original.remove(&flow.original);
        self.reply.remove(&flow.reply);
        if self.original.is_empty() {
            self.next_expiry = None;
        }
        flow
    }

    fn remove_masquerade_batch(
        &mut self,
        ifindex: u32,
        address: Option<CtAddress>,
        removed: &mut [Option<Arc<CtFlow>>; GC_BATCH],
    ) -> usize {
        let mut count = 0;
        self.original.retain(|_, flow| {
            if flow.masquerade_ifindex == Some(ifindex)
                && address.is_none_or(|address| flow.reply.dst == address)
                && count < GC_BATCH
            {
                removed[count] = Some(flow.clone());
                count += 1;
                false
            } else {
                true
            }
        });
        for flow in removed.iter().take(count).flatten() {
            let reply = self.reply.remove(&flow.reply);
            debug_assert!(reply.as_ref().is_some_and(|entry| Arc::ptr_eq(entry, flow)));
        }
        if self.original.is_empty() {
            self.next_expiry = None;
        }
        count
    }

    /// Remove only a conflicting expired flow from both indexes. This is
    /// bounded work on a packet path; bulk expiry remains poller-owned. The
    /// returned reference must outlive the table lock during destruction.
    fn purge_expired_key(&mut self, key: CtTuple, now: Instant) -> Option<Arc<CtFlow>> {
        let flow = self.original.get(&key).or_else(|| self.reply.get(&key))?;
        if flow.runtime.lock().expires_at > now {
            return None;
        }
        let flow = flow.clone();
        Some(self.remove_flow(flow))
    }

    fn remove_expired_batch(
        &mut self,
        now: Instant,
        evicted: &mut [Option<Arc<CtFlow>>; GC_BATCH],
    ) -> usize {
        let mut count = 0;
        let mut next_expiry: Option<Instant> = None;
        // Scan only in the namespace poller and cap both the table and the
        // number of removals per iteration. Retain an Arc outside the lock so
        // the last reference cannot run the flow destructor under this lock.
        self.original.retain(|_, flow| {
            let expiry = flow.runtime.lock().expires_at;
            if expiry <= now {
                if count < GC_BATCH {
                    evicted[count] = Some(flow.clone());
                    count += 1;
                    return false;
                }
                next_expiry = Some(now);
            } else {
                next_expiry = Some(next_expiry.map_or(expiry, |old| old.min(expiry)));
            }
            true
        });
        for flow in evicted.iter().take(count).flatten() {
            // The retained Arc above prevents a last-reference drop here.
            let removed = self.reply.remove(&flow.reply);
            debug_assert!(removed
                .as_ref()
                .is_some_and(|entry| Arc::ptr_eq(entry, flow)));
        }
        self.next_expiry = next_expiry;
        count
    }
}

/// Activation performs all hash-table allocation in process context, before
/// any packet can enter conntrack. Packet-path insertion never grows a map.
#[derive(Debug)]
pub(crate) struct CtState {
    table: SpinLock<Option<CtTable>>,
    active_families: AtomicU8,
}

impl CtState {
    pub(crate) const DEFAULT_MAX_FLOWS: usize = MAX_FLOWS;

    pub(crate) const fn new() -> Self {
        Self {
            table: SpinLock::new(None),
            active_families: AtomicU8::new(0),
        }
    }

    /// Runtime allocation remains after the last ruleset consumer is removed
    /// so pinned older snapshots can finish safely. Current hook registration
    /// is still governed by each ruleset snapshot. The atomic avoids taking
    /// the CT table lock just to select the packet path on every RX token.
    pub(crate) fn is_active(&self) -> bool {
        self.active_families.load(Ordering::Acquire) != 0
    }

    pub(crate) fn is_active_family(&self, version: IpVersion) -> bool {
        let bit = match version {
            IpVersion::Ipv4 => 1,
            IpVersion::Ipv6 => 2,
        };
        self.active_families.load(Ordering::Acquire) & bit != 0
    }

    pub(crate) fn activate(&self, max_flows: usize) -> Result<(), CtError> {
        self.activate_families(3, max_flows)
    }

    pub(crate) fn activate_family(
        &self,
        version: IpVersion,
        max_flows: usize,
    ) -> Result<(), CtError> {
        let bit = match version {
            IpVersion::Ipv4 => 1,
            IpVersion::Ipv6 => 2,
        };
        self.activate_families(bit, max_flows)
    }

    fn activate_families(&self, families: u8, max_flows: usize) -> Result<(), CtError> {
        if max_flows == 0 || max_flows > MAX_FLOWS {
            return Err(CtError::Invalid);
        }
        if self.active_families.load(Ordering::Acquire) & families == families {
            return Ok(());
        }
        // The flow indexes are shared by both IP families. Enabling the
        // second family must not allocate (or fail allocating) another pair
        // of maps that would only be discarded below.
        if self.table.lock().is_some() {
            self.active_families.fetch_or(families, Ordering::Release);
            return Ok(());
        }
        let mut original = HashMap::new();
        let mut reply = HashMap::new();
        original
            .try_reserve(max_flows)
            .map_err(|_| CtError::NoMemory)?;
        reply
            .try_reserve(max_flows)
            .map_err(|_| CtError::NoMemory)?;
        let prepared = CtTable {
            last_serial: 0,
            original,
            reply,
            max_flows,
            next_expiry: None,
            masquerade_generation: 0,
        };
        let mut guard = self.table.lock();
        if guard.is_none() {
            *guard = Some(prepared);
        }
        drop(guard);
        self.active_families.fetch_or(families, Ordering::Release);
        Ok(())
    }

    /// Classify after the family parser has validated a complete datagram.
    /// INVALID is a visible state for nft expressions, not an unconditional
    /// DROP; NAT will never map it. Existing flows are looked up before a
    /// private candidate is made, and ICMP errors never create new flows.
    pub(crate) fn classify(&self, parsed: ParsedCtPacket, now: Instant) -> CtPacketContext {
        if !self.is_active() {
            return CtPacketContext::Untracked;
        }
        match parsed {
            ParsedCtPacket::Untracked => CtPacketContext::Untracked,
            ParsedCtPacket::Invalid => CtPacketContext::Invalid,
            ParsedCtPacket::Related {
                quoted,
                outer_destination,
            } => self
                .related(quoted, outer_destination, now)
                .map_or(CtPacketContext::Invalid, CtPacketContext::Matched),
            ParsedCtPacket::Flow { tuple, kind } => {
                if let Some(found) = self.lookup(tuple, kind, now) {
                    // Linux clears skb->_nfct when the protocol tracker
                    // declares a packet INVALID. Keep the flow alive for
                    // subsequent valid packets, but do not expose its NAT,
                    // direction or tuple identity to this invalid packet.
                    return if found.state == CtPacketState::Invalid {
                        CtPacketContext::Invalid
                    } else {
                        CtPacketContext::Matched(found)
                    };
                }
                CtCandidate::new(tuple, kind)
                    .map(CtPacketContext::Candidate)
                    .unwrap_or(CtPacketContext::Invalid)
            }
        }
    }

    pub(crate) fn lookup(
        &self,
        tuple: CtTuple,
        kind: CtPacketKind,
        now: Instant,
    ) -> Option<CtMatch> {
        if !kind.matches(tuple) {
            return None;
        }
        let mut guard = self.table.lock();
        let table = guard.as_mut()?;
        // Linux inserts ORIGINAL then REPLY at the head of the same hash
        // bucket. For a self-symmetric tuple, later packets therefore find
        // REPLY first. Cross-direction collisions between distinct flows are
        // rejected by confirm(), so reply-first is otherwise equivalent.
        let (flow, direction) = if let Some(flow) = table.reply.get(&tuple) {
            (flow, CtDirection::Reply)
        } else {
            (table.original.get(&tuple)?, CtDirection::Original)
        };
        let mut runtime = flow.runtime.lock();
        if runtime.expires_at <= now {
            drop(runtime);
            let released = table.purge_expired_key(tuple, now);
            drop(guard);
            drop(released);
            return None;
        }
        let decision = runtime.observe(kind, direction, now);
        drop(runtime);
        if matches!(decision, FlowDecision::Repeat) {
            let released = table.remove_flow(flow.clone());
            drop(guard);
            drop(released);
            return None;
        }
        let (state, destroy) = match decision {
            FlowDecision::Matched { state, destroy } => (state, destroy),
            FlowDecision::Invalid => (CtPacketState::Invalid, false),
            FlowDecision::Repeat => unreachable!(),
        };
        let matched = CtMatch {
            flow: flow.clone(),
            direction,
            state,
        };
        if destroy {
            // `matched` keeps the last flow reference past lock release.
            table.remove_flow(matched.flow.clone());
        }
        Some(matched)
    }

    /// Select one provisional mapping for the first packet. `request` is
    /// already UAPI-validated and an explicit MASQUERADE address comes from
    /// the final egress. No candidate or packet is mutated on failure.
    pub(crate) fn select_nat_mapping(
        &self,
        candidate: &mut CtCandidate,
        side: NatManipSide,
        request: CtNatRequest,
        now: Instant,
    ) -> Result<CtNatRewrite, CtError> {
        if candidate.nat_initialized(side)
            || (request.masquerade_ifindex.is_some() && side != NatManipSide::Source)
            || request.masquerade_ifindex == Some(0)
            || request.address.is_some_and(|address| {
                core::mem::discriminant(&address)
                    != core::mem::discriminant(&candidate.original.src)
            })
            || (request.ports.is_some() && nat_port(candidate.translated, side).is_none())
        {
            return Err(CtError::Invalid);
        }

        let mut base = candidate.translated;
        if let Some(address) = request.address {
            match side {
                NatManipSide::Destination => base.dst = address,
                NatManipSide::Source => base.src = address,
            }
        }
        let prior_port = nat_port(base, side);
        let range = prior_port.and_then(|port| {
            request
                .ports
                .or_else(|| (side == NatManipSide::Source).then(|| implicit_port_range(port)))
        });
        let guard = self.table.lock();
        let table = guard.as_ref().ok_or(CtError::Inactive)?;
        let original_port_in_range = match (prior_port, range) {
            (Some(port), Some(range)) => port >= range.first && port <= range.last,
            (Some(_), None) => true,
            _ => false,
        };
        let mut selected = (original_port_in_range || prior_port.is_none())
            .then(|| table.nat_tuple_available(candidate.original, base, now))
            .filter(|available| *available)
            .map(|_| base);
        if selected.is_none() {
            if let Some(range) = range {
                let count = u32::from(range.last) - u32::from(range.first) + 1;
                let seed = nat_probe_seed(candidate.original, now);
                for (round, attempts) in NAT_PORT_ATTEMPTS.into_iter().enumerate() {
                    let offset = if count <= NAT_PORT_ATTEMPTS[0] {
                        0
                    } else {
                        seed.wrapping_add((round as u32).wrapping_mul(0x9e3779b9)) % count
                    };
                    for attempt in 0..attempts.min(count) {
                        let port = u32::from(range.first) + (offset + attempt) % count;
                        let proposed = with_nat_port(base, side, port as u16);
                        if table.nat_tuple_available(candidate.original, proposed, now) {
                            selected = Some(proposed);
                            break;
                        }
                    }
                    if selected.is_some() {
                        break;
                    }
                }
            }
        }
        let masquerade_generation = request
            .masquerade_ifindex
            .map(|_| table.masquerade_generation);
        drop(guard);
        let translated = selected.ok_or(CtError::Collision)?;
        let old = candidate.translated;
        candidate.set_translated(translated)?;
        candidate.mark_nat_initialized(side);
        candidate.masquerade_ifindex = request.masquerade_ifindex.or(candidate.masquerade_ifindex);
        if masquerade_generation.is_some() {
            candidate.masquerade_generation = masquerade_generation;
        }
        let plan = candidate.nat_rewrite(side);
        debug_assert_eq!(plan.to, translated);
        debug_assert!(old == plan.from || side == NatManipSide::Destination);
        Ok(plan)
    }

    /// ICMP errors quote the offending packet, but the error travels in the
    /// opposite direction. Match the inverse of that quote and require the
    /// outer destination to be the destination of the matched direction.
    /// ICMP quote rewriting is a separate packet-path responsibility.
    pub(crate) fn related(
        &self,
        quoted: CtTuple,
        outer_destination: CtAddress,
        now: Instant,
    ) -> Option<CtMatch> {
        let guard = self.table.lock();
        let table = guard.as_ref()?;
        let inverse = quoted.reverse()?;
        let (flow, direction) = table
            .reply
            .get(&inverse)
            .map(|f| (f, CtDirection::Reply))
            .or_else(|| {
                table
                    .original
                    .get(&inverse)
                    .map(|f| (f, CtDirection::Original))
            })?;
        if inverse.dst != outer_destination || flow.runtime.lock().expires_at <= now {
            return None;
        }
        Some(CtMatch {
            flow: flow.clone(),
            direction,
            state: CtPacketState::Related,
        })
    }

    pub(crate) fn confirm(
        &self,
        candidate: CtCandidate,
        now: Instant,
    ) -> Result<CtConfirm, CtError> {
        let reply_key = candidate.proposed_reply();
        // Fallible allocation is outside the spin lock. Activation already
        // reserved every map bucket that a successful insert could need.
        let runtime = FlowRuntime::new(candidate.kind, now);
        let expiry = runtime.expires_at;
        let flow = Arc::try_new(CtFlow {
            serial: AtomicU64::new(0),
            nat_done: (u32::from(candidate.source_initialized) << 7)
                | (u32::from(candidate.destination_initialized) << 8),
            original: candidate.original,
            translated: candidate.translated,
            reply: reply_key,
            masquerade_ifindex: candidate.masquerade_ifindex,
            runtime: SpinLock::new(runtime),
        })
        .map_err(|_| CtError::NoMemory)?;
        let mut expired: [Option<Arc<CtFlow>>; 2] = [None, None];
        let result = (|| {
            let mut guard = self.table.lock();
            let table = guard.as_mut().ok_or(CtError::Inactive)?;
            if candidate
                .masquerade_generation
                .is_some_and(|generation| generation != table.masquerade_generation)
            {
                return Err(CtError::Invalid);
            }
            expired[0] = table.purge_expired_key(candidate.original, now);
            if reply_key != candidate.original {
                expired[1] = table.purge_expired_key(reply_key, now);
            }
            // Cross-direction checks matter: a new original can equal another
            // flow's reply and vice versa. Own orig == reply is permitted.
            let collisions = [
                table.original.get(&candidate.original),
                table.reply.get(&candidate.original),
                table.original.get(&reply_key),
                table.reply.get(&reply_key),
            ];
            if let Some(existing) = collisions.into_iter().flatten().next() {
                if existing.original == candidate.original
                    && existing.translated == candidate.translated
                    && existing.reply == reply_key
                    && existing.masquerade_ifindex == candidate.masquerade_ifindex
                {
                    return Ok(CtConfirm::Reused(existing.clone()));
                }
                return Err(CtError::Collision);
            }
            if table.original.len() >= table.max_flows {
                return Err(CtError::Full);
            }
            // Keep dump traversal finite even while new flows are confirmed.
            // Unlike a u32 ID allocator this does not stop after 2^32 flows.
            table.last_serial = table.last_serial.checked_add(1).ok_or(CtError::Full)?;
            flow.serial.store(table.last_serial, Ordering::Relaxed);
            // Both maps were reserved to max_flows at activation; no allocation
            // or failure occurs between publishing their two keys.
            debug_assert!(table.original.capacity() >= table.max_flows);
            debug_assert!(table.reply.capacity() >= table.max_flows);
            table.original.insert(candidate.original, flow.clone());
            table.reply.insert(reply_key, flow.clone());
            table.next_expiry = Some(table.next_expiry.map_or(expiry, |old| old.min(expiry)));
            Ok(CtConfirm::Inserted(flow))
        })();
        drop(expired);
        result
    }

    pub(crate) fn next_expiry(&self) -> Option<Instant> {
        self.table
            .lock()
            .as_ref()
            .and_then(|table| table.next_expiry)
    }

    pub(crate) fn expire_due(&self, now: Instant) -> usize {
        // The poller calls this from process context. The fixed-size array
        // keeps the final Arc releases outside the table's spin lock without
        // allocating in the critical section.
        let mut evicted: [Option<Arc<CtFlow>>; GC_BATCH] = core::array::from_fn(|_| None);
        let mut guard = self.table.lock();
        let Some(table) = guard.as_mut() else {
            return 0;
        };
        if table.next_expiry.is_none_or(|expiry| expiry > now) {
            return 0;
        }
        let removed = table.remove_expired_batch(now, &mut evicted);
        drop(guard);
        drop(evicted);
        removed
    }

    /// Called from the interface/address lifecycle owner, outside packet and
    /// interface locks. Existing MASQUERADE replies must stop using an address
    /// after its egress disappears or changes. Each table lock hold removes at
    /// most one fixed batch; final Arc drops happen after the lock is released.
    pub(crate) fn invalidate_masquerade_oif(&self, ifindex: u32) -> usize {
        self.invalidate_masquerade(ifindex, None)
    }

    /// Linux's inet_cmp removes only flows whose MASQ egress ifindex matches
    /// this device and whose reply destination is the removed address. A
    /// fallback address from another interface is not keyed by its owner.
    pub(crate) fn invalidate_masquerade_address(&self, ifindex: u32, address: CtAddress) -> usize {
        self.invalidate_masquerade(ifindex, Some(address))
    }

    fn invalidate_masquerade(&self, ifindex: u32, address: Option<CtAddress>) -> usize {
        if ifindex == 0 {
            return 0;
        }
        let mut total = 0;
        let mut first_batch = true;
        loop {
            let mut released: [Option<Arc<CtFlow>>; GC_BATCH] = core::array::from_fn(|_| None);
            let count = {
                let mut guard = self.table.lock();
                guard.as_mut().map_or(0, |table| {
                    if first_batch {
                        table.masquerade_generation = table.masquerade_generation.wrapping_add(1);
                    }
                    table.remove_masquerade_batch(ifindex, address, &mut released)
                })
            };
            first_batch = false;
            total += count;
            drop(released);
            if count < GC_BATCH {
                return total;
            }
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.table
            .lock()
            .as_ref()
            .map_or(0, |table| table.original.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn tuple(src: [u8; 4], dst: [u8; 4], port: u16) -> CtTuple {
        CtTuple {
            src: src.into(),
            dst: dst.into(),
            l4: CtL4::Udp {
                src_port: port,
                dst_port: 80,
            },
        }
    }

    fn ipv4_udp_packet(tuple: CtTuple) -> alloc::vec::Vec<u8> {
        let (CtAddress::V4(src), CtAddress::V4(dst)) = (tuple.src, tuple.dst) else {
            panic!("IPv4 test tuple required")
        };
        let CtL4::Udp { src_port, dst_port } = tuple.l4 else {
            panic!("UDP test tuple required")
        };
        let mut bytes = vec![0u8; 28];
        bytes[0] = 0x45;
        bytes[2..4].copy_from_slice(&28u16.to_be_bytes());
        bytes[8] = 64;
        bytes[9] = 17;
        bytes[12..16].copy_from_slice(&src);
        bytes[16..20].copy_from_slice(&dst);
        bytes[20..22].copy_from_slice(&src_port.to_be_bytes());
        bytes[22..24].copy_from_slice(&dst_port.to_be_bytes());
        bytes[24..26].copy_from_slice(&8u16.to_be_bytes());
        let header_checksum = !packet::checksum(&bytes[..20], 0);
        bytes[10..12].copy_from_slice(&header_checksum.to_be_bytes());
        bytes
    }

    #[test]
    fn activation_is_independent_for_ipv4_and_ipv6() {
        let state = CtState::new();
        state.activate_family(IpVersion::Ipv4, 2).unwrap();
        assert!(state.is_active_family(IpVersion::Ipv4));
        assert!(!state.is_active_family(IpVersion::Ipv6));
        state.activate_family(IpVersion::Ipv6, 2).unwrap();
        assert!(state.is_active_family(IpVersion::Ipv6));
    }

    #[test]
    fn packet_context_keeps_new_flow_private_until_confirmation() {
        let table = CtState::new();
        assert!(!table.is_active());
        table.activate(2).unwrap();
        assert!(table.is_active());
        let key = tuple([192, 0, 2, 1], [198, 51, 100, 2], 40000);
        let parsed = ParsedCtPacket::Flow {
            tuple: key,
            kind: CtPacketKind::Udp,
        };
        let pending = table.classify(parsed, Instant::ZERO);
        assert_eq!(pending.state(), Some(CtPacketState::New));
        assert!(table
            .lookup(key, CtPacketKind::Udp, Instant::ZERO)
            .is_none());
        // Dropping a packet releases its candidate without publishing it.
        drop(pending);
        assert_eq!(table.len(), 0);
        let pending = table.classify(parsed, Instant::ZERO);
        table
            .confirm(pending.into_candidate().unwrap(), Instant::ZERO)
            .unwrap();
        assert!(matches!(
            table.classify(parsed, Instant::from_secs(1)),
            CtPacketContext::Matched(_)
        ));
        assert_eq!(table.len(), 1);
    }

    #[test]
    fn cloned_local_output_context_confirms_one_flow_for_two_copies() {
        let table = CtState::new();
        table.activate(2).unwrap();
        let key = tuple([192, 0, 2, 1], [224, 0, 0, 1], 40000);
        let context = table.classify(
            ParsedCtPacket::Flow {
                tuple: key,
                kind: CtPacketKind::Udp,
            },
            Instant::ZERO,
        );
        let local_copy = context.clone();
        assert!(matches!(
            table.confirm(local_copy.into_candidate().unwrap(), Instant::ZERO),
            Ok(CtConfirm::Inserted(_))
        ));
        assert!(matches!(
            table.confirm(context.into_candidate().unwrap(), Instant::ZERO),
            Ok(CtConfirm::Reused(_))
        ));
        assert_eq!(table.len(), 1);
    }

    #[test]
    fn packet_context_never_confirms_invalid_or_unmatched_error() {
        let table = CtState::new();
        table.activate(2).unwrap();
        assert_eq!(
            table
                .classify(ParsedCtPacket::Invalid, Instant::ZERO)
                .state(),
            Some(CtPacketState::Invalid)
        );
        assert_eq!(
            table
                .classify(ParsedCtPacket::Untracked, Instant::ZERO)
                .state(),
            None
        );
        let key = tuple([192, 0, 2, 1], [198, 51, 100, 2], 40000);
        let error = ParsedCtPacket::Related {
            quoted: key,
            outer_destination: key.src,
        };
        let context = table.classify(error, Instant::ZERO);
        assert_eq!(context.state(), Some(CtPacketState::Invalid));
        assert!(context.into_candidate().is_none());
        assert_eq!(table.len(), 0);
    }

    #[test]
    fn invalid_tcp_packet_cannot_borrow_an_existing_flows_identity() {
        let table = CtState::new();
        table.activate(2).unwrap();
        let key = CtTuple {
            src: [192, 0, 2, 1].into(),
            dst: [198, 51, 100, 2].into(),
            l4: CtL4::Tcp {
                src_port: 40000,
                dst_port: 80,
            },
        };
        let syn = TcpSegment {
            seq: 100,
            ack_seq: 0,
            window: 4096,
            payload_len: 0,
            syn: true,
            ack: false,
            fin: false,
            rst: false,
            urg: false,
            options: TcpOptions::default(),
        };
        let pending = table.classify(
            ParsedCtPacket::Flow {
                tuple: key,
                kind: CtPacketKind::Tcp(syn),
            },
            Instant::ZERO,
        );
        table
            .confirm(pending.into_candidate().unwrap(), Instant::ZERO)
            .unwrap();
        // FIN+ACK from the reply direction before the opening SYN/ACK is
        // INVALID in Linux's SYN_SENT transition table.
        let invalid_reply_fin = TcpSegment {
            seq: 200,
            syn: false,
            ack: true,
            ack_seq: 101,
            fin: true,
            ..syn
        };
        assert!(matches!(
            table.classify(
                ParsedCtPacket::Flow {
                    tuple: key.reverse().unwrap(),
                    kind: CtPacketKind::Tcp(invalid_reply_fin),
                },
                Instant::from_secs(1)
            ),
            CtPacketContext::Invalid
        ));
        assert_eq!(table.len(), 1);
    }

    #[test]
    fn generic_protocol_is_tracked_without_guessing_transport_ports() {
        let table = CtState::new();
        table.activate(2).unwrap();
        let key = CtTuple {
            src: [192, 0, 2, 1].into(),
            dst: [198, 51, 100, 2].into(),
            l4: CtL4::Generic { protocol: 47 },
        };
        let parsed = ParsedCtPacket::Flow {
            tuple: key,
            kind: CtPacketKind::Generic,
        };
        let candidate = table.classify(parsed, Instant::ZERO);
        assert_eq!(candidate.state(), Some(CtPacketState::New));
        table
            .confirm(candidate.into_candidate().unwrap(), Instant::ZERO)
            .unwrap();
        let reply = ParsedCtPacket::Flow {
            tuple: key.reverse().unwrap(),
            kind: CtPacketKind::Generic,
        };
        assert_eq!(
            table.classify(reply, Instant::from_secs(1)).state(),
            Some(CtPacketState::Established)
        );
        let distinct_protocol = ParsedCtPacket::Flow {
            tuple: CtTuple {
                l4: CtL4::Generic { protocol: 50 },
                ..key
            },
            kind: CtPacketKind::Generic,
        };
        assert_eq!(
            table
                .classify(distinct_protocol, Instant::from_secs(1))
                .state(),
            Some(CtPacketState::New)
        );
    }

    #[test]
    fn family_tag_prevents_ipv4_ipv6_alias_and_cross_family_nat() {
        let ipv4 = tuple([0, 0, 0, 1], [0, 0, 0, 2], 1234);
        let ipv6 = CtTuple {
            src: CtAddress::V6([0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
            dst: CtAddress::V6([0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
            l4: ipv4.l4,
        };
        assert_ne!(ipv4, ipv6);
        let mut candidate = CtCandidate::new(ipv4, CtPacketKind::Udp).unwrap();
        assert_eq!(candidate.set_translated(ipv6), Err(CtError::Invalid));
        let mixed = CtTuple {
            src: ipv4.src,
            dst: ipv6.dst,
            l4: ipv4.l4,
        };
        assert!(matches!(
            CtCandidate::new(mixed, CtPacketKind::Udp),
            Err(CtError::Invalid)
        ));
    }

    #[test]
    fn icmp_types_are_bound_to_their_ip_family() {
        let ipv6 = CtTuple {
            src: CtAddress::V6([0; 16]),
            dst: CtAddress::V6([1; 16]),
            l4: CtL4::Icmpv6 {
                identifier: 42,
                kind: 128,
                code: 0,
            },
        };
        assert_eq!(
            ipv6.reverse().unwrap().l4,
            CtL4::Icmpv6 {
                identifier: 42,
                kind: 129,
                code: 0,
            }
        );
        assert!(CtCandidate::new(ipv6, CtPacketKind::IcmpQuery).is_ok());
        assert!(matches!(
            CtCandidate::new(ipv6.reverse().unwrap(), CtPacketKind::IcmpQuery),
            Err(CtError::Invalid)
        ));
        let ni = CtTuple {
            l4: CtL4::Icmpv6 {
                identifier: 2,
                kind: 139,
                code: 0,
            },
            ..ipv6
        };
        assert!(CtCandidate::new(ni, CtPacketKind::IcmpQuery).is_ok());
        assert!(matches!(
            CtCandidate::new(
                CtTuple {
                    src: [10, 0, 0, 1].into(),
                    dst: [10, 0, 0, 2].into(),
                    ..ipv6
                },
                CtPacketKind::IcmpQuery
            ),
            Err(CtError::Invalid)
        ));
        assert!(matches!(
            CtCandidate::new(
                CtTuple {
                    l4: CtL4::Icmp {
                        identifier: 42,
                        kind: 8,
                        code: 0
                    },
                    ..ipv6
                },
                CtPacketKind::IcmpQuery
            ),
            Err(CtError::Invalid)
        ));
    }

    fn tcp_segment(
        seq: u32,
        ack_seq: u32,
        syn: bool,
        ack: bool,
        fin: bool,
        rst: bool,
    ) -> CtPacketKind {
        CtPacketKind::Tcp(TcpSegment {
            seq,
            ack_seq,
            window: 4096,
            payload_len: 0,
            syn,
            ack,
            fin,
            rst,
            urg: false,
            options: tcp::TcpOptions::default(),
        })
    }

    #[test]
    fn two_directions_share_one_mapping_and_expire_together() {
        let table = CtState::new();
        table.activate(2).unwrap();
        let original = tuple([10, 0, 0, 2], [203, 0, 113, 1], 1234);
        let mut candidate = CtCandidate::new(original, CtPacketKind::Udp).unwrap();
        let translated = tuple([192, 0, 2, 1], [10, 0, 0, 3], 50000);
        candidate.set_translated(translated).unwrap();
        let CtConfirm::Inserted(flow) = table.confirm(candidate, Instant::ZERO).unwrap() else {
            panic!()
        };
        assert_eq!(flow.original(), original);
        assert_eq!(flow.translated(), translated);
        let reply = translated.reverse().unwrap();
        let found = table
            .lookup(reply, CtPacketKind::Udp, Instant::from_secs(1))
            .unwrap();
        assert_eq!(found.direction, CtDirection::Reply);
        assert_eq!(found.state, CtPacketState::Established);
        assert!(Arc::ptr_eq(&flow, &found.flow));
        assert_eq!(table.expire_due(Instant::from_secs(122)), 1);
        assert_eq!(table.len(), 0);
        assert!(table
            .lookup(reply, CtPacketKind::Udp, Instant::from_secs(122))
            .is_none());
    }

    #[test]
    fn confirm_rechecks_both_indexes_and_does_not_publish_collision() {
        let table = CtState::new();
        table.activate(2).unwrap();
        let first = tuple([10, 0, 0, 2], [10, 0, 0, 3], 1234);
        let first_candidate = CtCandidate::new(first, CtPacketKind::Udp).unwrap();
        let second_candidate = CtCandidate::new(first, CtPacketKind::Udp).unwrap();
        assert!(matches!(
            table.confirm(first_candidate, Instant::ZERO),
            Ok(CtConfirm::Inserted(_))
        ));
        assert!(matches!(
            table.confirm(second_candidate, Instant::ZERO),
            Ok(CtConfirm::Reused(_))
        ));
        let reverse = first.reverse().unwrap();
        assert_eq!(
            table
                .confirm(
                    CtCandidate::new(reverse, CtPacketKind::Udp).unwrap(),
                    Instant::ZERO
                )
                .unwrap_err(),
            CtError::Collision
        );
        assert_eq!(table.len(), 1);
    }

    #[test]
    fn collision_after_nat_port_choice_is_rejected() {
        let table = CtState::new();
        table.activate(2).unwrap();
        let first = tuple([10, 0, 0, 2], [203, 0, 113, 1], 1234);
        let second = tuple([10, 0, 0, 3], [203, 0, 113, 1], 1234);
        let mut a = CtCandidate::new(first, CtPacketKind::Udp).unwrap();
        let mut b = CtCandidate::new(second, CtPacketKind::Udp).unwrap();
        let mapped = tuple([192, 0, 2, 1], [203, 0, 113, 1], 50000);
        a.set_translated(mapped).unwrap();
        b.set_translated(mapped).unwrap();
        table.confirm(a, Instant::ZERO).unwrap();
        assert_eq!(
            table.confirm(b, Instant::ZERO).unwrap_err(),
            CtError::Collision
        );
        assert_eq!(table.len(), 1);
    }

    #[test]
    fn capacity_and_expiry_allow_port_reuse() {
        let table = CtState::new();
        table.activate(1).unwrap();
        let first = tuple([10, 0, 0, 2], [203, 0, 113, 1], 1234);
        let second = tuple([10, 0, 0, 3], [203, 0, 113, 1], 1234);
        table
            .confirm(
                CtCandidate::new(first, CtPacketKind::Udp).unwrap(),
                Instant::ZERO,
            )
            .unwrap();
        assert_eq!(
            table
                .confirm(
                    CtCandidate::new(second, CtPacketKind::Udp).unwrap(),
                    Instant::ZERO
                )
                .unwrap_err(),
            CtError::Full
        );
        assert_eq!(table.expire_due(Instant::from_secs(31)), 1);
        assert!(matches!(
            table.confirm(
                CtCandidate::new(second, CtPacketKind::Udp).unwrap(),
                Instant::from_secs(31)
            ),
            Ok(CtConfirm::Inserted(_))
        ));
    }

    #[test]
    fn symmetric_tuple_has_no_cross_flow_ambiguity() {
        let table = CtState::new();
        table.activate(1).unwrap();
        let self_tuple = CtTuple {
            src: [127, 0, 0, 1].into(),
            dst: [127, 0, 0, 1].into(),
            l4: CtL4::Udp {
                src_port: 5555,
                dst_port: 5555,
            },
        };
        let flow = table
            .confirm(
                CtCandidate::new(self_tuple, CtPacketKind::Udp).unwrap(),
                Instant::ZERO,
            )
            .unwrap();
        assert!(matches!(flow, CtConfirm::Inserted(_)));
        assert_eq!(
            table
                .lookup(self_tuple, CtPacketKind::Udp, Instant::from_secs(1))
                .unwrap()
                .direction,
            CtDirection::Reply
        );
        assert_eq!(table.expire_due(Instant::from_secs(121)), 1);
    }

    #[test]
    fn gc_is_bounded_and_expires_all_indexes_in_batches() {
        let table = CtState::new();
        assert_eq!(table.activate(MAX_FLOWS + 1), Err(CtError::Invalid));
        table.activate(GC_BATCH + 1).unwrap();
        for port in 1000..(1000 + GC_BATCH as u16 + 1) {
            let original = tuple([10, 0, 0, 2], [203, 0, 113, 1], port);
            table
                .confirm(
                    CtCandidate::new(original, CtPacketKind::Udp).unwrap(),
                    Instant::ZERO,
                )
                .unwrap();
        }
        assert_eq!(table.expire_due(Instant::from_secs(31)), GC_BATCH);
        assert_eq!(table.len(), 1);
        assert_eq!(table.next_expiry(), Some(Instant::from_secs(31)));
        assert_eq!(table.expire_due(Instant::from_secs(31)), 1);
        assert_eq!(table.len(), 0);
        assert_eq!(table.next_expiry(), None);
    }

    #[test]
    fn expired_flow_is_removed_from_both_indexes_on_lookup() {
        let table = CtState::new();
        table.activate(1).unwrap();
        let original = tuple([10, 0, 0, 2], [203, 0, 113, 1], 1234);
        let reply = original.reverse().unwrap();
        table
            .confirm(
                CtCandidate::new(original, CtPacketKind::Udp).unwrap(),
                Instant::ZERO,
            )
            .unwrap();
        assert!(table
            .lookup(reply, CtPacketKind::Udp, Instant::from_secs(31))
            .is_none());
        assert_eq!(table.len(), 0);
        assert!(matches!(
            table.confirm(
                CtCandidate::new(original, CtPacketKind::Udp).unwrap(),
                Instant::from_secs(31)
            ),
            Ok(CtConfirm::Inserted(_))
        ));
    }

    #[test]
    fn confirm_reclaims_only_expired_colliding_keys() {
        let table = CtState::new();
        table.activate(1).unwrap();
        let original = tuple([10, 0, 0, 2], [203, 0, 113, 1], 1234);
        table
            .confirm(
                CtCandidate::new(original, CtPacketKind::Udp).unwrap(),
                Instant::ZERO,
            )
            .unwrap();
        assert!(matches!(
            table.confirm(
                CtCandidate::new(original, CtPacketKind::Udp).unwrap(),
                Instant::from_secs(31)
            ),
            Ok(CtConfirm::Inserted(_))
        ));
        assert_eq!(table.len(), 1);
    }

    #[test]
    fn icmp_error_uses_inverse_quote_and_checks_outer_destination() {
        let table = CtState::new();
        table.activate(2).unwrap();
        let original = tuple([10, 0, 0, 2], [203, 0, 113, 1], 1234);
        let translated = tuple([192, 0, 2, 1], [203, 0, 113, 1], 50000);
        let mut candidate = CtCandidate::new(original, CtPacketKind::Udp).unwrap();
        candidate.set_translated(translated).unwrap();
        table.confirm(candidate, Instant::ZERO).unwrap();
        let error = table
            .related(translated, translated.src, Instant::from_secs(1))
            .unwrap();
        assert_eq!(error.direction, CtDirection::Reply);
        assert_eq!(error.state, CtPacketState::Related);
        assert!(table
            .related(translated, [192, 0, 2, 99].into(), Instant::from_secs(1))
            .is_none());
        assert!(table
            .related(original, original.dst, Instant::from_secs(1))
            .is_none());

        let other = tuple([10, 0, 0, 3], [203, 0, 113, 2], 4567);
        table
            .confirm(
                CtCandidate::new(other, CtPacketKind::Udp).unwrap(),
                Instant::ZERO,
            )
            .unwrap();
        let reverse_quote = other.reverse().unwrap();
        let error = table
            .related(reverse_quote, other.dst, Instant::from_secs(1))
            .unwrap();
        assert_eq!(error.direction, CtDirection::Original);
    }

    #[test]
    fn live_table_can_move_into_deferred_teardown_without_allocation() {
        let mut state = CtState::new();
        state.activate(1).unwrap();
        let original = tuple([10, 0, 0, 2], [203, 0, 113, 1], 1234);
        state
            .confirm(
                CtCandidate::new(original, CtPacketKind::Udp).unwrap(),
                Instant::ZERO,
            )
            .unwrap();
        let deferred = core::mem::replace(&mut state, CtState::new());
        assert_eq!(state.len(), 0);
        assert_eq!(deferred.len(), 1);
        drop(deferred);
    }

    #[test]
    fn reply_reset_does_not_pin_old_tcp_mapping_for_next_syn() {
        let table = CtState::new();
        table.activate(1).unwrap();
        let tuple = CtTuple {
            src: [10, 0, 0, 2].into(),
            dst: [203, 0, 113, 1].into(),
            l4: CtL4::Tcp {
                src_port: 1234,
                dst_port: 80,
            },
        };
        let syn = tcp_segment(100, 0, true, false, false, false);
        table
            .confirm(CtCandidate::new(tuple, syn).unwrap(), Instant::ZERO)
            .unwrap();
        table
            .lookup(
                tuple.reverse().unwrap(),
                tcp_segment(0, 101, false, true, false, true),
                Instant::from_secs(1),
            )
            .unwrap();
        assert_eq!(table.len(), 0);
        assert!(matches!(
            table.confirm(CtCandidate::new(tuple, syn).unwrap(), Instant::from_secs(2)),
            Ok(CtConfirm::Inserted(_))
        ));
    }

    #[test]
    fn opening_syn_retransmit_does_not_extend_unreplied_flow_lifetime() {
        let table = CtState::new();
        table.activate(1).unwrap();
        let tuple = CtTuple {
            src: [10, 0, 0, 2].into(),
            dst: [203, 0, 113, 1].into(),
            l4: CtL4::Tcp {
                src_port: 1234,
                dst_port: 80,
            },
        };
        let syn = tcp_segment(100, 0, true, false, false, false);
        table
            .confirm(CtCandidate::new(tuple, syn).unwrap(), Instant::ZERO)
            .unwrap();
        assert!(table.lookup(tuple, syn, Instant::from_secs(119)).is_some());
        assert_eq!(table.expire_due(Instant::from_secs(121)), 1);
        assert_eq!(table.len(), 0);
    }

    #[test]
    fn icmp_query_reverse_and_tcp_lifecycle() {
        let echo = CtTuple {
            src: [10, 0, 0, 2].into(),
            dst: [10, 0, 0, 3].into(),
            l4: CtL4::Icmp {
                identifier: 7,
                kind: 8,
                code: 0,
            },
        };
        assert_eq!(
            echo.reverse().unwrap().l4,
            CtL4::Icmp {
                identifier: 7,
                kind: 0,
                code: 0
            }
        );
        let table = CtState::new();
        table.activate(1).unwrap();
        let tcp = CtTuple {
            src: [10, 0, 0, 2].into(),
            dst: [10, 0, 0, 3].into(),
            l4: CtL4::Tcp {
                src_port: 1111,
                dst_port: 80,
            },
        };
        let syn = tcp_segment(100, 0, true, false, false, false);
        table
            .confirm(CtCandidate::new(tcp, syn).unwrap(), Instant::ZERO)
            .unwrap();
        assert_eq!(
            table.lookup(tcp, syn, Instant::from_secs(1)).unwrap().state,
            CtPacketState::New
        );
        let reply = tcp.reverse().unwrap();
        let synack = tcp_segment(500, 101, true, true, false, false);
        assert_eq!(
            table
                .lookup(reply, synack, Instant::from_secs(2))
                .unwrap()
                .state,
            CtPacketState::Established
        );
        assert_eq!(
            table
                .lookup(
                    tcp,
                    tcp_segment(101, 501, false, true, false, false),
                    Instant::from_secs(3)
                )
                .unwrap()
                .state,
            CtPacketState::Established
        );
    }

    #[test]
    fn nat_null_binding_is_once_per_side_and_does_not_change_tuple() {
        let original = tuple([10, 0, 0, 2], [203, 0, 113, 1], 1234);
        let mut candidate = CtCandidate::new(original, CtPacketKind::Udp).unwrap();
        assert!(!candidate.nat_initialized(NatManipSide::Destination));
        candidate
            .initialize_null_binding(NatManipSide::Destination)
            .unwrap();
        assert_eq!(
            candidate.initialize_null_binding(NatManipSide::Destination),
            Err(CtError::Invalid)
        );
        candidate
            .initialize_null_binding(NatManipSide::Source)
            .unwrap();
        assert_eq!(candidate.translated(), original);
        assert_eq!(
            candidate.nat_rewrite(NatManipSide::Destination),
            CtNatRewrite {
                from: original,
                to: original
            }
        );
    }

    #[test]
    fn selected_dnat_and_snat_rewrite_each_direction_at_its_outer_hook() {
        let table = CtState::new();
        table.activate(2).unwrap();
        let original = tuple([10, 0, 0, 2], [203, 0, 113, 1], 1234);
        let mut candidate = CtCandidate::new(original, CtPacketKind::Udp).unwrap();
        let dnat = CtNatRequest::new(
            Some([10, 0, 0, 3].into()),
            Some(CtNatPortRange::new(8080, 8080).unwrap()),
        )
        .unwrap();
        let pre = table
            .select_nat_mapping(
                &mut candidate,
                NatManipSide::Destination,
                dnat,
                Instant::ZERO,
            )
            .unwrap();
        assert_eq!(pre.from, original);
        assert_eq!(pre.to.dst, [10, 0, 0, 3].into());
        assert_eq!(nat_port(pre.to, NatManipSide::Destination), Some(8080));
        let snat = CtNatRequest::new(
            Some([192, 0, 2, 1].into()),
            Some(CtNatPortRange::new(50000, 50000).unwrap()),
        )
        .unwrap();
        let post = table
            .select_nat_mapping(&mut candidate, NatManipSide::Source, snat, Instant::ZERO)
            .unwrap();
        assert_eq!(post.from, pre.to);
        assert_eq!(post.to.src, [192, 0, 2, 1].into());
        assert_eq!(nat_port(post.to, NatManipSide::Source), Some(50000));
        let CtConfirm::Inserted(flow) = table.confirm(candidate, Instant::ZERO).unwrap() else {
            panic!()
        };
        assert_eq!(
            flow.nat_rewrite(CtDirection::Original, NatManipSide::Destination),
            pre
        );
        assert_eq!(
            flow.nat_rewrite(CtDirection::Original, NatManipSide::Source),
            post
        );
        let reply_start = post.to.reverse().unwrap();
        let reply_pre = flow.nat_rewrite(CtDirection::Reply, NatManipSide::Destination);
        let reply_post = flow.nat_rewrite(CtDirection::Reply, NatManipSide::Source);
        assert_eq!(reply_pre.from, reply_start);
        assert_eq!(reply_pre.to.dst, original.src);
        assert_eq!(
            nat_port(reply_pre.to, NatManipSide::Destination),
            Some(1234)
        );
        assert_eq!(reply_post.from, reply_pre.to);
        assert_eq!(reply_post.to, original.reverse().unwrap());
    }

    #[test]
    fn confirmed_nat_context_rewrites_both_directions_without_reselecting_rules() {
        let table = CtState::new();
        table.activate(2).unwrap();
        let original = tuple([10, 0, 0, 2], [203, 0, 113, 1], 1234);
        let mut candidate = CtCandidate::new(original, CtPacketKind::Udp).unwrap();
        table
            .select_nat_mapping(
                &mut candidate,
                NatManipSide::Destination,
                CtNatRequest::new(Some([10, 0, 0, 3].into()), None).unwrap(),
                Instant::ZERO,
            )
            .unwrap();
        table
            .select_nat_mapping(
                &mut candidate,
                NatManipSide::Source,
                CtNatRequest::new(Some([192, 0, 2, 1].into()), None).unwrap(),
                Instant::ZERO,
            )
            .unwrap();
        let CtConfirm::Inserted(flow) = table.confirm(candidate, Instant::ZERO).unwrap() else {
            panic!()
        };
        let outgoing = CtPacketContext::Matched(CtMatch {
            flow: flow.clone(),
            direction: CtDirection::Original,
            state: CtPacketState::Established,
        });
        let mut packet = ipv4_udp_packet(original);
        assert!(outgoing
            .rewrite_confirmed_nat(
                &mut packet,
                NatManipSide::Destination,
                CtChecksumMode::Verify,
            )
            .unwrap());
        assert!(outgoing
            .rewrite_confirmed_nat(&mut packet, NatManipSide::Source, CtChecksumMode::Verify)
            .unwrap());
        assert!(matches!(
            parse_ipv4_conntrack(&packet),
            ParsedCtPacket::Flow { tuple, .. } if tuple == flow.translated()
        ));

        let incoming = CtPacketContext::Matched(CtMatch {
            flow,
            direction: CtDirection::Reply,
            state: CtPacketState::Established,
        });
        let mut reply = ipv4_udp_packet(incoming_match_translated(&incoming).reverse().unwrap());
        incoming
            .rewrite_confirmed_nat(
                &mut reply,
                NatManipSide::Destination,
                CtChecksumMode::Verify,
            )
            .unwrap();
        incoming
            .rewrite_confirmed_nat(&mut reply, NatManipSide::Source, CtChecksumMode::Verify)
            .unwrap();
        assert!(matches!(
            parse_ipv4_conntrack(&reply),
            ParsedCtPacket::Flow { tuple, .. } if tuple == original.reverse().unwrap()
        ));
    }

    #[test]
    fn one_sided_nat_replies_use_the_other_outer_hook_without_a_rule_chain() {
        for side in [NatManipSide::Destination, NatManipSide::Source] {
            let table = CtState::new();
            table.activate(2).unwrap();
            let original = tuple([10, 0, 0, 2], [203, 0, 113, 1], 1234);
            let mut candidate = CtCandidate::new(original, CtPacketKind::Udp).unwrap();
            let address = match side {
                NatManipSide::Destination => [10, 0, 0, 3].into(),
                NatManipSide::Source => [192, 0, 2, 1].into(),
            };
            table
                .select_nat_mapping(
                    &mut candidate,
                    side,
                    CtNatRequest::new(Some(address), None).unwrap(),
                    Instant::ZERO,
                )
                .unwrap();
            candidate
                .initialize_null_binding(match side {
                    NatManipSide::Destination => NatManipSide::Source,
                    NatManipSide::Source => NatManipSide::Destination,
                })
                .unwrap();
            let CtConfirm::Inserted(flow) = table.confirm(candidate, Instant::ZERO).unwrap() else {
                panic!()
            };
            let incoming = CtPacketContext::Matched(CtMatch {
                flow: flow.clone(),
                direction: CtDirection::Reply,
                state: CtPacketState::Established,
            });
            let mut packet = ipv4_udp_packet(flow.translated().reverse().unwrap());
            incoming
                .rewrite_confirmed_nat(
                    &mut packet,
                    NatManipSide::Destination,
                    CtChecksumMode::Verify,
                )
                .unwrap();
            incoming
                .rewrite_confirmed_nat(&mut packet, NatManipSide::Source, CtChecksumMode::Verify)
                .unwrap();
            assert!(matches!(
                parse_ipv4_conntrack(&packet),
                ParsedCtPacket::Flow { tuple, .. } if tuple == original.reverse().unwrap()
            ));
        }
    }

    #[test]
    fn same_original_race_cannot_reuse_a_different_translated_mapping() {
        let table = CtState::new();
        table.activate(2).unwrap();
        let original = tuple([10, 0, 0, 2], [203, 0, 113, 1], 1234);
        let mut first = CtCandidate::new(original, CtPacketKind::Udp).unwrap();
        let mut racing = CtCandidate::new(original, CtPacketKind::Udp).unwrap();
        for (candidate, mapped) in [(&mut first, [10, 0, 0, 3]), (&mut racing, [10, 0, 0, 4])] {
            table
                .select_nat_mapping(
                    candidate,
                    NatManipSide::Destination,
                    CtNatRequest::new(Some(mapped.into()), None).unwrap(),
                    Instant::ZERO,
                )
                .unwrap();
        }
        table.confirm(first, Instant::ZERO).unwrap();
        assert!(matches!(
            table.confirm(racing, Instant::ZERO),
            Err(CtError::Collision)
        ));
    }

    fn incoming_match_translated(context: &CtPacketContext) -> CtTuple {
        let CtPacketContext::Matched(found) = context else {
            panic!()
        };
        found.flow.translated()
    }

    #[test]
    fn nat_port_choice_checks_both_indexes_and_confirm_rechecks_races() {
        let table = CtState::new();
        table.activate(4).unwrap();
        let first = tuple([10, 0, 0, 2], [203, 0, 113, 1], 1234);
        let second = tuple([10, 0, 0, 3], [203, 0, 113, 1], 1234);
        let request = CtNatRequest::new(
            Some([192, 0, 2, 1].into()),
            Some(CtNatPortRange::new(50000, 50001).unwrap()),
        )
        .unwrap();
        let mut a = CtCandidate::new(first, CtPacketKind::Udp).unwrap();
        let mut b = CtCandidate::new(second, CtPacketKind::Udp).unwrap();
        table
            .select_nat_mapping(&mut a, NatManipSide::Source, request, Instant::ZERO)
            .unwrap();
        table
            .select_nat_mapping(&mut b, NatManipSide::Source, request, Instant::ZERO)
            .unwrap();
        assert_eq!(a.translated().l4, b.translated().l4);
        table.confirm(a, Instant::ZERO).unwrap();
        assert!(matches!(
            table.confirm(b, Instant::ZERO),
            Err(CtError::Collision)
        ));
        let mut retry = CtCandidate::new(second, CtPacketKind::Udp).unwrap();
        table
            .select_nat_mapping(
                &mut retry,
                NatManipSide::Source,
                request,
                Instant::from_secs(1),
            )
            .unwrap();
        assert_eq!(
            nat_port(retry.translated(), NatManipSide::Source),
            Some(50001)
        );
        assert!(matches!(
            table.confirm(retry, Instant::from_secs(1)),
            Ok(CtConfirm::Inserted(_))
        ));
    }

    #[test]
    fn nat_port_choice_also_rejects_an_existing_original_key() {
        let table = CtState::new();
        table.activate(2).unwrap();
        let occupied = CtTuple {
            src: [203, 0, 113, 1].into(),
            dst: [192, 0, 2, 1].into(),
            l4: CtL4::Udp {
                src_port: 80,
                dst_port: 50000,
            },
        };
        table
            .confirm(
                CtCandidate::new(occupied, CtPacketKind::Udp).unwrap(),
                Instant::ZERO,
            )
            .unwrap();
        let original = tuple([10, 0, 0, 2], [203, 0, 113, 1], 1234);
        let mut candidate = CtCandidate::new(original, CtPacketKind::Udp).unwrap();
        let request = CtNatRequest::new(
            Some([192, 0, 2, 1].into()),
            Some(CtNatPortRange::new(50000, 50001).unwrap()),
        )
        .unwrap();
        table
            .select_nat_mapping(&mut candidate, NatManipSide::Source, request, Instant::ZERO)
            .unwrap();
        assert_eq!(
            nat_port(candidate.translated(), NatManipSide::Source),
            Some(50001)
        );
    }

    #[test]
    fn implicit_port_classes_and_destination_nat_do_not_change_unspecified_port() {
        assert_eq!(implicit_port_range(0), CtNatPortRange::new(1, 511).unwrap());
        assert_eq!(
            implicit_port_range(511),
            CtNatPortRange::new(1, 511).unwrap()
        );
        assert_eq!(
            implicit_port_range(512),
            CtNatPortRange::new(600, 1023).unwrap()
        );
        assert_eq!(
            implicit_port_range(1023),
            CtNatPortRange::new(600, 1023).unwrap()
        );
        assert_eq!(
            implicit_port_range(1024),
            CtNatPortRange::new(1024, 65535).unwrap()
        );

        let table = CtState::new();
        table.activate(2).unwrap();
        let original = tuple([10, 0, 0, 2], [203, 0, 113, 1], 1234);
        let target = [10, 0, 0, 3];
        let mut first = CtCandidate::new(original, CtPacketKind::Udp).unwrap();
        table
            .select_nat_mapping(
                &mut first,
                NatManipSide::Destination,
                CtNatRequest::new(Some(target.into()), None).unwrap(),
                Instant::ZERO,
            )
            .unwrap();
        table.confirm(first, Instant::ZERO).unwrap();
        let mut collision = CtCandidate::new(
            tuple([10, 0, 0, 2], [203, 0, 113, 2], 1234),
            CtPacketKind::Udp,
        )
        .unwrap();
        assert_eq!(
            table.select_nat_mapping(
                &mut collision,
                NatManipSide::Destination,
                CtNatRequest::new(Some(target.into()), None).unwrap(),
                Instant::ZERO,
            ),
            Err(CtError::Collision)
        );
    }

    #[test]
    fn ipv6_node_information_qtype_is_not_a_nat_port() {
        let table = CtState::new();
        table.activate_family(IpVersion::Ipv6, 1).unwrap();
        let original = CtTuple {
            src: CtAddress::V6([1; 16]),
            dst: CtAddress::V6([2; 16]),
            l4: CtL4::Icmpv6 {
                identifier: 2,
                kind: 139,
                code: 0,
            },
        };
        let mut candidate = CtCandidate::new(original, CtPacketKind::IcmpQuery).unwrap();
        let address_only = CtNatRequest::new(Some(CtAddress::V6([3; 16])), None).unwrap();
        table
            .select_nat_mapping(
                &mut candidate,
                NatManipSide::Source,
                address_only,
                Instant::ZERO,
            )
            .unwrap();
        assert_eq!(candidate.translated().l4, original.l4);
        let ports = CtNatRequest::new(None, Some(CtNatPortRange::new(3, 3).unwrap())).unwrap();
        let mut second = CtCandidate::new(original, CtPacketKind::IcmpQuery).unwrap();
        assert_eq!(
            table.select_nat_mapping(&mut second, NatManipSide::Source, ports, Instant::ZERO),
            Err(CtError::Invalid)
        );
    }

    #[test]
    fn masquerade_requires_source_side_and_records_egress() {
        let table = CtState::new();
        table.activate(1).unwrap();
        let original = tuple([10, 0, 0, 2], [203, 0, 113, 1], 1234);
        let request = CtNatRequest::masquerade([192, 0, 2, 1].into(), 7, None).unwrap();
        let mut candidate = CtCandidate::new(original, CtPacketKind::Udp).unwrap();
        assert_eq!(
            table.select_nat_mapping(
                &mut candidate,
                NatManipSide::Destination,
                request,
                Instant::ZERO
            ),
            Err(CtError::Invalid)
        );
        table
            .select_nat_mapping(&mut candidate, NatManipSide::Source, request, Instant::ZERO)
            .unwrap();
        let CtConfirm::Inserted(flow) = table.confirm(candidate, Instant::ZERO).unwrap() else {
            panic!()
        };
        assert_eq!(flow.masquerade_ifindex(), Some(7));
        assert_eq!(
            CtNatRequest::masquerade([192, 0, 2, 1].into(), 0, None),
            Err(CtError::Invalid)
        );
        assert_eq!(table.invalidate_masquerade_oif(8), 0);
        assert_eq!(table.invalidate_masquerade_oif(7), 1);
        assert!(table
            .lookup(flow.reply(), CtPacketKind::Udp, Instant::from_secs(1))
            .is_none());
    }

    #[test]
    fn masquerade_candidate_cannot_confirm_after_interface_invalidation() {
        let table = CtState::new();
        table.activate(1).unwrap();
        let original = tuple([10, 0, 0, 2], [203, 0, 113, 1], 1234);
        let mut candidate = CtCandidate::new(original, CtPacketKind::Udp).unwrap();
        let request = CtNatRequest::masquerade([192, 0, 2, 1].into(), 7, None).unwrap();
        table
            .select_nat_mapping(&mut candidate, NatManipSide::Source, request, Instant::ZERO)
            .unwrap();
        assert_eq!(table.invalidate_masquerade_oif(7), 0);
        assert!(matches!(
            table.confirm(candidate, Instant::ZERO),
            Err(CtError::Invalid)
        ));
        assert_eq!(table.len(), 0);
    }

    #[test]
    fn masquerade_address_removal_only_evicts_flows_using_that_reply_target() {
        let table = CtState::new();
        table.activate(3).unwrap();
        let mut flows = alloc::vec::Vec::new();
        for (source, mapped, egress) in [
            ([10, 0, 0, 2], [192, 0, 2, 1], 7),
            ([10, 0, 0, 3], [192, 0, 2, 2], 7),
            ([10, 0, 0, 4], [192, 0, 2, 3], 8),
        ] {
            let mut candidate =
                CtCandidate::new(tuple(source, [203, 0, 113, 1], 1234), CtPacketKind::Udp).unwrap();
            table
                .select_nat_mapping(
                    &mut candidate,
                    NatManipSide::Source,
                    CtNatRequest::masquerade(mapped.into(), egress, None).unwrap(),
                    Instant::ZERO,
                )
                .unwrap();
            let CtConfirm::Inserted(flow) = table.confirm(candidate, Instant::ZERO).unwrap() else {
                panic!()
            };
            flows.push(flow);
        }
        assert_eq!(
            table.invalidate_masquerade_address(7, [192, 0, 2, 1].into()),
            1
        );
        assert!(table
            .lookup(flows[0].reply(), CtPacketKind::Udp, Instant::ZERO)
            .is_none());
        assert!(table
            .lookup(flows[1].reply(), CtPacketKind::Udp, Instant::ZERO)
            .is_some());
        assert!(table
            .lookup(flows[2].reply(), CtPacketKind::Udp, Instant::ZERO)
            .is_some());
        assert_eq!(
            table.invalidate_masquerade_address(8, [192, 0, 2, 2].into()),
            0
        );
        assert!(table
            .lookup(flows[1].reply(), CtPacketKind::Udp, Instant::ZERO)
            .is_some());
        assert_eq!(
            table.invalidate_masquerade_address(7, [192, 0, 2, 2].into()),
            1
        );
        assert!(table
            .lookup(flows[1].reply(), CtPacketKind::Udp, Instant::ZERO)
            .is_none());
        assert!(table
            .lookup(flows[2].reply(), CtPacketKind::Udp, Instant::ZERO)
            .is_some());
    }

    #[test]
    fn related_quote_plan_requires_expected_stage_tuple() {
        let table = CtState::new();
        table.activate(1).unwrap();
        let original = tuple([10, 0, 0, 2], [203, 0, 113, 1], 1234);
        let translated = CtTuple {
            src: [192, 0, 2, 1].into(),
            dst: [10, 0, 0, 3].into(),
            l4: CtL4::Udp {
                src_port: 50000,
                dst_port: 8080,
            },
        };
        let mut candidate = CtCandidate::new(original, CtPacketKind::Udp).unwrap();
        candidate.set_translated(translated).unwrap();
        table.confirm(candidate, Instant::ZERO).unwrap();
        let error = table
            .related(translated, translated.src, Instant::from_secs(1))
            .unwrap();
        let pre = error
            .related_quote_rewrite(NatManipSide::Destination, translated)
            .unwrap();
        assert_eq!(pre.to.src, original.src);
        assert_eq!(nat_port(pre.to, NatManipSide::Source), Some(1234));
        assert!(error
            .related_quote_rewrite(NatManipSide::Source, translated)
            .is_none());
        let post = error
            .related_quote_rewrite(NatManipSide::Source, pre.to)
            .unwrap();
        assert_eq!(post.to, original);
    }
}
