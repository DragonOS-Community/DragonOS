pub(super) use super::deferred_queue::DeferredRouteKey;
use super::deferred_queue::{DeferredRouteLimits, DeferredRouteQueue, JoinDeferredResult};
use super::*;
use crate::libs::prepared_queue::{Node as PreparedNode, Queue as PreparedQueue};
use crate::net::conntrack::CtPacketContext;
use core::cell::RefCell;

pub(super) const MAX_LOCAL_OUTPUT_FRAMES: usize = 1024;
pub(super) type OutputNode = alloc::boxed::Box<PreparedNode<OutputEnvelope>>;
pub(super) type OutputQueue = PreparedQueue<OutputEnvelope>;
const _: () = assert!(
    core::mem::size_of::<PreparedNode<OutputEnvelope>>() <= crate::libs::paged_vec::CHUNK_BYTES
);
const _: () = assert!(
    core::mem::size_of::<PreparedNode<LocalInputPacket>>() <= crate::libs::paged_vec::CHUNK_BYTES
);

/// One admission node follows its packet through ready, backpressure and
/// neighbor queues. Only an uncommitted reservation may have no packet.
#[derive(Debug)]
pub(super) struct OutputEnvelope {
    pub(super) packet: Option<LocalOutputPacket>,
    pub(super) retry_at: smoltcp::time::Instant,
}

impl OutputEnvelope {
    fn empty() -> Self {
        Self {
            packet: None,
            retry_at: smoltcp::time::Instant::from_micros(0),
        }
    }

    pub(super) fn packet(&self) -> &LocalOutputPacket {
        self.packet
            .as_ref()
            .expect("published output node has a packet")
    }

    pub(super) fn packet_mut(&mut self) -> &mut LocalOutputPacket {
        self.packet
            .as_mut()
            .expect("published output node has a packet")
    }
}

impl core::ops::Deref for OutputEnvelope {
    type Target = LocalOutputPacket;
    fn deref(&self) -> &Self::Target {
        self.packet()
    }
}

impl core::ops::DerefMut for OutputEnvelope {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.packet_mut()
    }
}

/// Optional accounting owner for a queued output datagram. The networking
/// queue knows only that bytes were charged, not which socket/account owns
/// them. Dropping the last packet or an uncommitted reservation releases the
/// charge exactly once.
pub(crate) trait OutputCompletion: Send + Sync {
    fn output_complete(&self, charged_bytes: usize);
}

pub(crate) struct OutputCharge {
    owner: Arc<dyn OutputCompletion>,
    charged_bytes: usize,
}

impl OutputCharge {
    pub(crate) fn new(owner: Arc<dyn OutputCompletion>, charged_bytes: usize) -> Self {
        Self {
            owner,
            charged_bytes,
        }
    }
}

impl fmt::Debug for OutputCharge {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OutputCharge")
            .field("charged_bytes", &self.charged_bytes)
            .finish_non_exhaustive()
    }
}

impl Drop for OutputCharge {
    fn drop(&mut self) {
        self.owner.output_complete(self.charged_bytes);
    }
}

#[cfg(test)]
mod charge_tests {
    use super::*;

    struct Counter(AtomicUsize);

    impl OutputCompletion for Counter {
        fn output_complete(&self, charged_bytes: usize) {
            self.0.fetch_add(charged_bytes, Ordering::Relaxed);
        }
    }

    #[test]
    fn moving_a_charge_releases_it_only_once() {
        let owner = Arc::new(Counter(AtomicUsize::new(0)));
        let first = OutputCharge::new(owner.clone(), 42);
        let moved = Some(first);
        assert_eq!(owner.0.load(Ordering::Relaxed), 0);
        drop(moved);
        assert_eq!(owner.0.load(Ordering::Relaxed), 42);
    }
}

pub(super) struct LocalInputRxToken<'a> {
    pub(super) frame: PacketBuffer,
    pub(super) meta: PacketMeta,
    pub(super) ingress_stage: IngressStage,
    pub(super) stage_cell: Option<&'a Cell<IngressStage>>,
    pub(super) broadcast: bool,
    pub(super) broadcast_cell: Option<&'a Cell<bool>>,
    pub(super) ct_context: Option<CtPacketContext>,
    pub(super) ct_context_cell: Option<&'a RefCell<Option<CtPacketContext>>>,
    pub(super) mark: u32,
    pub(super) mark_cell: Option<&'a Cell<u32>>,
    pub(super) forward_fragment: Option<crate::net::forward_mtu::ReassembledForwardInfo>,
    pub(super) forward_fragment_cell:
        Option<&'a Cell<Option<crate::net::forward_mtu::ReassembledForwardInfo>>>,
}

struct IngressStageScope<'a> {
    cell: &'a Cell<IngressStage>,
    previous: IngressStage,
}

struct BroadcastScope<'a> {
    cell: &'a Cell<bool>,
    previous: bool,
}

impl Drop for BroadcastScope<'_> {
    fn drop(&mut self) {
        self.cell.set(self.previous);
    }
}

impl Drop for IngressStageScope<'_> {
    fn drop(&mut self) {
        self.cell.set(self.previous);
    }
}

struct CtContextScope<'a> {
    cell: &'a RefCell<Option<CtPacketContext>>,
}

impl Drop for CtContextScope<'_> {
    fn drop(&mut self) {
        // smoltcp does not nest receive-token consumption. A context left by
        // this packet must never become the next packet's identity.
        self.cell.borrow_mut().take();
    }
}

impl RxToken for LocalInputRxToken<'_> {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        let _ct_scope = self.ct_context_cell.map(|cell| {
            // Discard any context left by an earlier physical token before
            // installing this handoff's state. No prior packet is resumed.
            cell.replace(self.ct_context);
            CtContextScope { cell }
        });
        let _stage_scope = self.stage_cell.map(|cell| IngressStageScope {
            cell,
            previous: cell.replace(self.ingress_stage),
        });
        let _broadcast_scope = self.broadcast_cell.map(|cell| BroadcastScope {
            cell,
            previous: cell.replace(self.broadcast),
        });
        let _mark_scope = self.mark_cell.map(|cell| MarkScope {
            cell,
            previous: cell.replace(self.mark),
        });
        let _fragment_scope = self.forward_fragment_cell.map(|cell| ForwardFragmentScope {
            cell,
            previous: cell.replace(self.forward_fragment),
        });
        f(&self.frame)
    }

    fn meta(&self) -> PacketMeta {
        self.meta
    }
}

struct ForwardFragmentScope<'a> {
    cell: &'a Cell<Option<crate::net::forward_mtu::ReassembledForwardInfo>>,
    previous: Option<crate::net::forward_mtu::ReassembledForwardInfo>,
}

impl Drop for ForwardFragmentScope<'_> {
    fn drop(&mut self) {
        self.cell.set(self.previous);
    }
}

struct MarkScope<'a> {
    cell: &'a Cell<u32>,
    previous: u32,
}

impl Drop for MarkScope<'_> {
    fn drop(&mut self) {
        self.cell.set(self.previous);
    }
}

#[cfg(test)]
mod local_input_context_tests {
    use super::*;

    #[test]
    fn prerouted_token_owns_context_only_during_consume() {
        let stage = Cell::new(IngressStage::Pending);
        let broadcast = Cell::new(false);
        let context = RefCell::new(Some(CtPacketContext::Invalid));
        LocalInputRxToken {
            frame: PacketBuffer::new(),
            meta: PacketMeta::default(),
            ingress_stage: IngressStage::PreRoutingDone,
            stage_cell: Some(&stage),
            broadcast: true,
            broadcast_cell: Some(&broadcast),
            ct_context: Some(CtPacketContext::Untracked),
            ct_context_cell: Some(&context),
            mark: 0,
            mark_cell: None,
            forward_fragment: None,
            forward_fragment_cell: None,
        }
        .consume(|_| {
            assert_eq!(stage.get(), IngressStage::PreRoutingDone);
            assert!(broadcast.get());
            assert!(matches!(
                context.borrow().as_ref(),
                Some(CtPacketContext::Untracked)
            ));
        });
        assert_eq!(stage.get(), IngressStage::Pending);
        assert!(!broadcast.get());
        assert!(context.borrow().is_none());
    }

    #[test]
    fn token_without_context_does_not_restore_previous_packet() {
        let context = RefCell::new(Some(CtPacketContext::Invalid));
        LocalInputRxToken {
            frame: PacketBuffer::new(),
            meta: PacketMeta::default(),
            ingress_stage: IngressStage::LocalOutput,
            stage_cell: None,
            broadcast: false,
            broadcast_cell: None,
            ct_context: None,
            ct_context_cell: Some(&context),
            mark: 0,
            mark_cell: None,
            forward_fragment: None,
            forward_fragment_cell: None,
        }
        .consume(|_| assert!(context.borrow().is_none()));
        assert!(context.borrow().is_none());
    }

    #[test]
    fn local_token_installs_mark_only_during_consume() {
        let mark = Cell::new(0);
        LocalInputRxToken {
            frame: PacketBuffer::new(),
            meta: PacketMeta::default(),
            ingress_stage: IngressStage::LocalOutput,
            stage_cell: None,
            broadcast: false,
            broadcast_cell: None,
            ct_context: None,
            ct_context_cell: None,
            mark: 0x1234_5678,
            mark_cell: Some(&mark),
            forward_fragment: None,
            forward_fragment_cell: None,
        }
        .consume(|_| assert_eq!(mark.get(), 0x1234_5678));
        assert_eq!(mark.get(), 0);
    }
}

#[derive(Debug)]
pub(super) struct LocalInputPacket {
    pub(super) ingress_ifindex: u32,
    pub(super) ingress_stage: IngressStage,
    pub(super) broadcast: bool,
    pub(super) destination_mac: smoltcp::wire::EthernetAddress,
    pub(super) source_mac: smoltcp::wire::EthernetAddress,
    pub(super) ip_packet: PacketBuffer,
    pub(super) ct_context: Option<CtPacketContext>,
    pub(super) mark: u32,
    pub(super) forward_fragment: Option<crate::net::forward_mtu::ReassembledForwardInfo>,
}

impl LocalInputPacket {
    pub(super) fn len(&self) -> usize {
        self.ip_packet.len()
    }

    fn resident_bytes(&self) -> usize {
        self.ip_packet.capacity()
    }

    pub(super) fn into_frame(
        self,
        medium: smoltcp::phy::Medium,
    ) -> Result<PacketBuffer, SystemError> {
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
        let mut frame = PacketBuffer::new();
        frame
            .try_reserve_exact(frame_len)
            .map_err(|_| SystemError::ENOMEM)?;
        frame.extend_from_slice(&self.destination_mac.0)?;
        frame.extend_from_slice(&self.source_mac.0)?;
        frame.extend_from_slice(&ethertype)?;
        frame.extend_from_slice(&self.ip_packet)?;
        Ok(frame)
    }
}

#[derive(Debug)]
pub(super) struct LocalInputQueueState {
    pub(super) packets: PreparedQueue<LocalInputPacket>,
    pub(super) bytes: usize,
}

#[derive(Debug)]
pub(super) struct LocalOutputPacket {
    pub(super) medium: smoltcp::phy::Medium,
    pub(super) meta: PacketMeta,
    pub(super) disposition: LocalOutputDisposition,
    pub(super) frame: PacketBuffer,
    /// Only tracked packets allocate a sidecar. Keeping the large candidate
    /// off the queue header preserves its normal no-CT footprint and avoids
    /// inflating every output/deferred-queue enum variant.
    pub(super) ct_context: OutputCtContext,
    /// Packet metadata survives deferred routing, fragmentation and retry.
    pub(super) mark: u32,
    /// Present only for an admitted complete datagram. Fragment progress
    /// advances after the device accepts a fragment, never on retry.
    pub(super) prepared_ip: Option<PreparedIpProgress>,
    /// Only transit packets with a non-fragmentable PMTU boundary own this.
    pub(super) forward_mtu_feedback: Option<Arc<crate::net::forward_mtu::ForwardMtuFeedback>>,
    pub(super) _charge: Option<OutputCharge>,
}

/// An in-flight output datagram must preserve the tracking decision made by
/// the ruleset pinned at admission. `Untracked` has no heap cost; `Tracked`
/// owns exactly this packet's identity across asynchronous TX retries.
/// There is deliberately no `None`: a missing identity in a tracked packet
/// would otherwise be indistinguishable from a packet admitted before CT was
/// enabled and could be misclassified after a ruleset change.
#[derive(Debug)]
pub(crate) enum OutputCtContext {
    Untracked,
    Tracked(alloc::boxed::Box<CtPacketContext>),
}

impl OutputCtContext {
    pub(crate) fn for_ingress(&self) -> CtPacketContext {
        match self {
            Self::Untracked => CtPacketContext::Untracked,
            Self::Tracked(context) => (**context).clone(),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(super) enum PreparedIpProgress {
    Ipv4 {
        offset: usize,
        may_fragment: bool,
        max_fragment_len: Option<usize>,
    },
    Ipv6 {
        offset: usize,
        identification: u32,
        max_fragment_len: Option<usize>,
    },
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
    pub(super) packets: OutputQueue,
    pub(super) backpressured: OutputQueue,
    pub(super) deferred_routes: DeferredRouteQueue,
    pub(super) frames: usize,
    pub(super) bytes: usize,
    pub(super) reserved_frames: usize,
    pub(super) reserved_bytes: usize,
}

#[derive(Debug, Default)]
pub(super) struct LocalOutputScratchPool {
    pub(super) buffers: Vec<PacketBuffer>,
    pub(super) bytes: usize,
}

#[derive(Debug)]
pub(super) struct LocalInputQueue {
    pub(super) state: SpinLock<LocalInputQueueState>,
    pub(super) response_scratch: SpinLock<LocalOutputScratchPool>,
    pub(super) output: SpinLock<LocalOutputQueueState>,
    pub(super) output_draining: AtomicBool,
    /// A netns move retires queued work before the device can be polled in
    /// its new namespace. Admission stays closed throughout that transition.
    accepting: AtomicBool,
}

pub(super) struct LocalOutputDrainGuard<'a> {
    pub(super) draining: &'a AtomicBool,
    pub(super) active: bool,
}

enum OutputQueueRef<'a> {
    Borrowed(&'a SpinLock<LocalOutputQueueState>),
    Owned(Arc<dyn Iface>),
}
impl core::ops::Deref for OutputQueueRef<'_> {
    type Target = SpinLock<LocalOutputQueueState>;
    fn deref(&self) -> &Self::Target {
        match self {
            Self::Borrowed(output) => output,
            Self::Owned(iface) => &iface.common().local_input_queue.output,
        }
    }
}

pub(super) struct LocalOutputReservation<'a> {
    output: OutputQueueRef<'a>,
    pub(super) bytes: usize,
    pub(super) active: bool,
    node: Option<OutputNode>,
}

#[expect(
    clippy::large_enum_variant,
    reason = "transfer the existing packet and receipt without a poll-path allocation"
)]
pub(super) enum LocalOutputPop<'a> {
    Ready(
        LocalOutputPacket,
        LocalOutputReservation<'a>,
        Option<DeferredRouteKey>,
    ),
    DeferredUntil(smoltcp::time::Instant),
    Empty,
}

// This is a small, short-lived stack carrier. Boxing Ready would introduce a
// fallible allocation after the queue has already admitted the packet.
const _: () = assert!(core::mem::size_of::<LocalOutputPop<'static>>() <= 256);

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
    fn install_packet(slot: &mut Option<OutputNode>, packet: LocalOutputPacket) -> OutputNode {
        let mut node = slot.take().expect("output reservation owns its node");
        debug_assert!(node.value().packet.is_none());
        node.value_mut().packet = Some(packet);
        node
    }

    fn restore_node(slot: &mut Option<OutputNode>, mut node: OutputNode) -> LocalOutputPacket {
        let packet = node
            .value_mut()
            .packet
            .take()
            .expect("output node owns packet");
        debug_assert!(slot.is_none());
        *slot = Some(node);
        packet
    }

    /// Move this token's byte reservation without changing the queue-wide
    /// frame reservation. Prepare the actual capacity before buffer birth.
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
            ct_context: OutputCtContext::Untracked,
            mark: 0,
            prepared_ip: None,
            forward_mtu_feedback: None,
            _charge: None,
        });
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "packet metadata is explicit at queue commit"
    )]
    pub(super) fn commit_prepared_ipv4(
        self,
        frame: PacketBuffer,
        meta: PacketMeta,
        disposition: LocalOutputDisposition,
        may_fragment: bool,
        charge: Option<OutputCharge>,
        ct_context: OutputCtContext,
        mark: u32,
    ) {
        self.commit_packet(LocalOutputPacket {
            medium: smoltcp::phy::Medium::Ip,
            meta,
            disposition,
            frame,
            ct_context,
            mark,
            prepared_ip: Some(PreparedIpProgress::Ipv4 {
                offset: 0,
                may_fragment,
                max_fragment_len: None,
            }),
            forward_mtu_feedback: None,
            _charge: charge,
        });
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "packet metadata is explicit at queue commit"
    )]
    pub(super) fn commit_prepared_ipv6(
        self,
        frame: PacketBuffer,
        meta: PacketMeta,
        disposition: LocalOutputDisposition,
        identification: u32,
        charge: Option<OutputCharge>,
        ct_context: OutputCtContext,
        mark: u32,
    ) {
        self.commit_packet(LocalOutputPacket {
            medium: smoltcp::phy::Medium::Ip,
            meta,
            disposition,
            frame,
            ct_context,
            mark,
            prepared_ip: Some(PreparedIpProgress::Ipv6 {
                offset: 0,
                identification,
                max_fragment_len: None,
            }),
            forward_mtu_feedback: None,
            _charge: charge,
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
        let mut node = Self::install_packet(&mut self.node, packet);
        node.value_mut().retry_at = retry_at;
        if let Some(index) = output
            .backpressured
            .iter()
            .position(|queued| queued.retry_at > retry_at)
        {
            let mut later = output.backpressured.split_off(index);
            output.backpressured.push_node(node);
            output.backpressured.append(&mut later);
        } else {
            output.backpressured.push_node(node);
        }
        self.active = false;
    }

    pub(super) fn requeue_native_backpressured(
        self,
        medium: smoltcp::phy::Medium,
        meta: PacketMeta,
        frame: PacketBuffer,
        retry_at: smoltcp::time::Instant,
    ) {
        self.requeue_backpressured(
            LocalOutputPacket {
                medium,
                meta,
                disposition: LocalOutputDisposition::NativeOwner,
                frame,
                ct_context: OutputCtContext::Untracked,
                mark: 0,
                prepared_ip: None,
                forward_mtu_feedback: None,
                _charge: None,
            },
            retry_at,
        );
    }

    #[expect(
        clippy::result_large_err,
        reason = "requeue failure must return the owned packet without a second allocation"
    )]
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
        let node = Self::install_packet(&mut self.node, packet);
        let mut output = self.output.lock();
        output.reserved_frames -= 1;
        output.reserved_bytes -= self.bytes;
        output.frames += 1;
        output.bytes += self.bytes;
        output.packets.push_node(node);
        self.active = false;
    }

    /// Keep the same reservation across successive fragments. No second
    /// allocation or target-interface queue admission is allowed here.
    pub(super) fn requeue_ready(self, packet: LocalOutputPacket) {
        self.commit_packet(packet);
    }

    #[expect(
        clippy::result_large_err,
        reason = "deferred admission failure preserves packet ownership and queue accounting"
    )]
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
        let node = Self::install_packet(&mut self.node, packet);
        let mut output = self.output.lock();
        if let Err(node) = output.deferred_routes.try_enqueue(
            node,
            self.bytes,
            retry_at,
            probe_sent,
            advance_existing_probe,
            LocalInputQueue::DEFERRED_ROUTE_LIMITS,
        ) {
            return Err(Self::restore_node(&mut self.node, node));
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
        let LocalOutputDisposition::Routed { .. } = packet.disposition else {
            return ExistingDeferredCommit::Missing(packet, self);
        };
        debug_assert_eq!(packet.frame.capacity(), self.bytes);
        let node = Self::install_packet(&mut self.node, packet);
        let mut output = self.output.lock();
        let retry_at = match output.deferred_routes.try_join(
            node,
            self.bytes,
            LocalInputQueue::DEFERRED_ROUTE_LIMITS,
        ) {
            JoinDeferredResult::Queued(retry_at) => retry_at,
            JoinDeferredResult::Missing(node) => {
                let packet = Self::restore_node(&mut self.node, node);
                drop(output);
                return ExistingDeferredCommit::Missing(packet, self);
            }
            JoinDeferredResult::Full(node) => {
                let packet = Self::restore_node(&mut self.node, node);
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

    #[expect(
        clippy::result_large_err,
        reason = "probe completion failure must return the owned packet"
    )]
    pub(super) fn finish_deferred_probe(
        mut self,
        packet: LocalOutputPacket,
        key: DeferredRouteKey,
        retry_at: smoltcp::time::Instant,
        probe_sent: bool,
    ) -> Result<bool, LocalOutputPacket> {
        debug_assert_eq!(packet.frame.capacity(), self.bytes);
        let node = Self::install_packet(&mut self.node, packet);
        let mut output = self.output.lock();
        let LocalOutputQueueState {
            packets,
            deferred_routes,
            ..
        } = &mut *output;
        let resolved = match deferred_routes
            .finish_probe(node, self.bytes, key, retry_at, probe_sent, packets)
        {
            Ok(resolved) => resolved,
            Err(node) => return Err(Self::restore_node(&mut self.node, node)),
        };
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
    const MAX_FRAMES: usize = MAX_LOCAL_OUTPUT_FRAMES;
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
                packets: PreparedQueue::new(),
                bytes: 0,
            }),
            response_scratch: SpinLock::new(LocalOutputScratchPool::default()),
            output: SpinLock::new(LocalOutputQueueState::default()),
            output_draining: AtomicBool::new(false),
            accepting: AtomicBool::new(true),
        }
    }

    pub(super) fn enqueue(
        &self,
        packet: LocalInputPacket,
        owner_still_current: impl FnOnce() -> bool,
    ) -> Result<(), SystemError> {
        let node = PreparedNode::try_new(packet)?;
        let mut state = self.state.lock();
        if !self.accepting.load(Ordering::Acquire) || !owner_still_current() {
            return Err(SystemError::ENODEV);
        }
        if state.packets.len() >= Self::MAX_FRAMES
            || state.bytes.saturating_add(node.value().resident_bytes()) > Self::MAX_BYTES
        {
            return Err(SystemError::ENOBUFS);
        }
        state.bytes += node.value().resident_bytes();
        state.packets.push_node(node);
        Ok(())
    }

    pub(super) fn pop(&self) -> Option<LocalInputPacket> {
        let mut state = self.state.lock();
        let packet = state.packets.pop_node()?.into_value();
        state.bytes -= packet.resident_bytes();
        Some(packet)
    }

    pub(super) fn is_empty(&self) -> bool {
        self.state.lock().packets.is_empty()
    }

    /// Quiesce an interface's namespace-local work before its ownership is
    /// changed. A prepared output reservation may outlive NAPI quiescence,
    /// so stop new admission and wait for every such reservation to finish
    /// before discarding the old namespace's queued packets.
    pub(super) fn retire_for_netns_move(&self) {
        self.accepting.store(false, Ordering::Release);
        // Serialize with readers that passed the admission check before the
        // store; neither input nor output can enter after these locks pass.
        drop(self.state.lock());
        drop(self.output.lock());
        while self.output.lock().reserved_frames != 0
            || self.output_draining.load(Ordering::Acquire)
        {
            crate::sched::sched_yield();
        }
        let input = {
            let mut state = self.state.lock();
            state.bytes = 0;
            core::mem::take(&mut state.packets)
        };
        let output = core::mem::take(&mut *self.output.lock());
        // OutputCharge drops may wake socket writers; never do that under a
        // queue spinlock or while holding any namespace routing lock.
        drop(input);
        drop(output);
    }

    pub(super) fn activate_after_netns_move(&self) {
        self.accepting.store(true, Ordering::Release);
    }

    /// Retain the interface only while a token owns an unpublished receipt.
    /// commit puts just its packet/node into the queue, never this Arc.
    pub(super) fn reserve_owned_output(
        iface: Arc<dyn Iface>,
    ) -> Option<LocalOutputReservation<'static>> {
        let mut reservation = iface.common().local_input_queue.reserve_output()?;
        let bytes = reservation.bytes;
        let active = reservation.active;
        let node = reservation.node.take();
        reservation.active = false;
        drop(reservation);
        Some(LocalOutputReservation {
            output: OutputQueueRef::Owned(iface),
            bytes,
            active,
            node,
        })
    }

    pub(super) fn reserve_output(&self) -> Option<LocalOutputReservation<'_>> {
        let mut output = self.output.lock();
        if !self.accepting.load(Ordering::Acquire) {
            return None;
        }
        if output.frames.saturating_add(output.reserved_frames) >= Self::MAX_FRAMES {
            return None;
        }
        let node = PreparedNode::try_new(OutputEnvelope::empty()).ok()?;
        let frames = output.frames + output.reserved_frames + 1;
        output.deferred_routes.prepare_capacity(frames).ok()?;
        output.reserved_frames += 1;
        Some(LocalOutputReservation {
            output: OutputQueueRef::Borrowed(&self.output),
            bytes: 0,
            active: true,
            node: Some(node),
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
                .pop_node()
                .expect("front was observed above");
            output.packets.push_node(queued);
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
        } else if let Some(packet) = output.packets.pop_node() {
            (Some(packet), None)
        } else {
            (None, None)
        };
        if let Some(mut node) = packet {
            let packet = node
                .value_mut()
                .packet
                .take()
                .expect("queued output owns packet");
            let bytes = packet.frame.capacity();
            output.frames -= 1;
            output.bytes -= bytes;
            output.reserved_frames += 1;
            output.reserved_bytes += bytes;
            return LocalOutputPop::Ready(
                packet,
                LocalOutputReservation {
                    output: OutputQueueRef::Borrowed(&self.output),
                    bytes,
                    active: true,
                    node: Some(node),
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
        let mut released = core::mem::take(&mut output.backpressured);
        output.packets.append(&mut released);
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
        if !self.accepting.load(Ordering::Acquire) {
            return None;
        }
        self.output_draining
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .ok()?;
        if !self.accepting.load(Ordering::Acquire) {
            self.output_draining.store(false, Ordering::Release);
            return None;
        }
        Some(LocalOutputDrainGuard {
            draining: &self.output_draining,
            active: true,
        })
    }

    pub(super) fn finish_output_drain(&self, guard: LocalOutputDrainGuard<'_>) -> bool {
        guard.finish_and_has_output(&self.output)
    }

    pub(super) fn recycle_output(&self, frame: PacketBuffer) {
        Self::recycle_scratch(&self.response_scratch, frame);
    }

    pub(super) fn recycle_scratch(
        pool: &SpinLock<LocalOutputScratchPool>,
        mut frame: PacketBuffer,
    ) {
        if frame.capacity() > crate::libs::paged_vec::CHUNK_BYTES {
            return;
        }

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

#[cfg(feature = "mm_selftest")]
pub(crate) fn input_residency_selftest() -> bool {
    fn packet(capacity: usize) -> Result<LocalInputPacket, SystemError> {
        let mut bytes = PacketBuffer::try_with_capacity(capacity)?;
        bytes.resize(capacity.min(20), 0xa5)?;
        Ok(LocalInputPacket {
            ingress_ifindex: 1,
            ingress_stage: IngressStage::Pending,
            broadcast: false,
            destination_mac: smoltcp::wire::EthernetAddress([0; 6]),
            source_mac: smoltcp::wire::EthernetAddress([0; 6]),
            ip_packet: bytes,
            ct_context: None,
            mark: 0,
            forward_fragment: None,
        })
    }
    fn run() -> Result<bool, SystemError> {
        let queue = LocalInputQueue::new();
        let input = packet(LocalInputQueue::MAX_BYTES)?;
        let pointer = input.ip_packet.as_ptr();
        queue.enqueue(input, || true)?;
        let resident = queue.state.lock().bytes;
        if resident != LocalInputQueue::MAX_BYTES
            || !matches!(
                queue.enqueue(packet(1)?, || true),
                Err(SystemError::ENOBUFS)
            )
        {
            return Ok(false);
        }
        let input = queue.pop().ok_or(SystemError::EIO)?;
        if input.ip_packet.as_ptr() != pointer
            || input.len() != 20
            || input.resident_bytes() != LocalInputQueue::MAX_BYTES
            || input.ip_packet.iter().any(|byte| *byte != 0xa5)
            || queue.state.lock().bytes != 0
        {
            return Ok(false);
        }
        drop(input);
        queue.enqueue(packet(1)?, || true)?;
        drop(queue.pop().ok_or(SystemError::EIO)?);
        let state = queue.state.lock();
        Ok(state.bytes == 0 && state.packets.is_empty())
    }
    run().unwrap_or(false)
}

#[cfg(test)]
mod output_ct_context_tests {
    use super::*;

    #[test]
    fn prepared_fragment_completion_releases_other_neighbor_packets() {
        let queue = LocalInputQueue::new();
        let next_hop = smoltcp::wire::IpAddress::Ipv6(smoltcp::wire::Ipv6Address::from([
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1,
        ]));
        let now = smoltcp::time::Instant::from_millis(0);
        for (mark, retry_ms) in [(1, 1000), (2, 2000)] {
            let frame = PacketBuffer::from_existing_heap(alloc::vec![0x60; 40]);
            let mut reservation = queue.reserve_output().unwrap();
            assert!(reservation.try_resize(frame.capacity()));
            reservation
                .commit_deferred_packet(
                    LocalOutputPacket {
                        medium: smoltcp::phy::Medium::Ip,
                        meta: PacketMeta::default(),
                        disposition: LocalOutputDisposition::Routed {
                            oif: 7,
                            next_hop,
                            ip_mtu: 1280,
                        },
                        frame,
                        ct_context: OutputCtContext::Untracked,
                        mark,
                        prepared_ip: Some(PreparedIpProgress::Ipv6 {
                            offset: 1200,
                            identification: 42,
                            max_fragment_len: None,
                        }),
                        forward_mtu_feedback: None,
                        _charge: None,
                    },
                    now + smoltcp::time::Duration::from_millis(retry_ms),
                    mark == 1,
                    false,
                )
                .unwrap();
        }
        // A later datagram must not move the existing probe deadline.
        assert!(matches!(
            queue.pop_ready_output(now, true),
            LocalOutputPop::DeferredUntil(deadline)
                if deadline == now + smoltcp::time::Duration::from_millis(1000)
        ));
        let due = now + smoltcp::time::Duration::from_millis(1000);
        let LocalOutputPop::Ready(fragment, reservation, Some(key)) =
            queue.pop_ready_output(due, true)
        else {
            panic!("the first prepared datagram must own the probe");
        };
        assert_eq!(fragment.mark, 1);
        // This is the Continue branch after the first fragment was sent.
        reservation.requeue_ready(fragment);
        queue.complete_deferred_probe_success(key);
        assert!(!queue.has_deferred_output());
        for expected_mark in [1, 2] {
            let LocalOutputPop::Ready(packet, reservation, None) =
                queue.pop_ready_output(due, false)
            else {
                panic!("fragment continuation and its peer must both progress");
            };
            assert_eq!(packet.mark, expected_mark);
            assert!(matches!(
                packet.prepared_ip,
                Some(PreparedIpProgress::Ipv6 { offset: 1200, .. })
            ));
            drop(packet);
            drop(reservation);
        }
        assert!(!queue.has_output());
    }

    #[test]
    fn deferred_output_keeps_its_packet_context_across_retry() {
        let queue = LocalInputQueue::new();
        let mut reservation = queue.reserve_output().unwrap();
        let mut frame = PacketBuffer::new();
        frame.try_reserve_exact(20).unwrap();
        frame.resize(20, 0).unwrap();
        assert!(reservation.try_resize(frame.capacity()));
        reservation.commit_packet(LocalOutputPacket {
            medium: smoltcp::phy::Medium::Ip,
            meta: PacketMeta::default(),
            disposition: LocalOutputDisposition::Local {
                oif: 1,
                ip_mtu: 1500,
            },
            frame,
            ct_context: OutputCtContext::Tracked(alloc::boxed::Box::new(CtPacketContext::Invalid)),
            mark: 0x1234_5678,
            prepared_ip: None,
            forward_mtu_feedback: None,
            _charge: None,
        });

        let now = smoltcp::time::Instant::from_millis(0);
        let LocalOutputPop::Ready(packet, reservation, _) = queue.pop_ready_output(now, false)
        else {
            panic!("committed packet must be ready")
        };
        assert!(matches!(
            &packet.ct_context,
            OutputCtContext::Tracked(context) if matches!(**context, CtPacketContext::Invalid)
        ));
        assert_eq!(packet.mark, 0x1234_5678);
        let retry_at = now + smoltcp::time::Duration::from_millis(1);
        reservation.requeue_backpressured(packet, retry_at);
        assert!(matches!(
            queue.pop_ready_output(now, false),
            LocalOutputPop::DeferredUntil(_)
        ));
        let LocalOutputPop::Ready(packet, reservation, _) = queue.pop_ready_output(retry_at, false)
        else {
            panic!("retried packet must be ready")
        };
        assert!(matches!(
            &packet.ct_context,
            OutputCtContext::Tracked(context) if matches!(**context, CtPacketContext::Invalid)
        ));
        assert_eq!(packet.mark, 0x1234_5678);
        drop(reservation);
    }

    #[test]
    fn prepared_ipv6_keeps_tracking_context_across_retry() {
        let queue = LocalInputQueue::new();
        let mut reservation = queue.reserve_output().unwrap();
        let frame = PacketBuffer::from_existing_heap(alloc::vec![0x60; 40]);
        assert!(reservation.try_resize(frame.capacity()));
        reservation.commit_prepared_ipv6(
            frame,
            PacketMeta::default(),
            LocalOutputDisposition::Local {
                oif: 1,
                ip_mtu: 1500,
            },
            42,
            None,
            OutputCtContext::Tracked(alloc::boxed::Box::new(CtPacketContext::Invalid)),
            0x8765_4321,
        );
        let now = smoltcp::time::Instant::from_millis(0);
        let LocalOutputPop::Ready(packet, reservation, _) = queue.pop_ready_output(now, false)
        else {
            panic!("prepared packet must be ready")
        };
        assert!(matches!(
            &packet.ct_context,
            OutputCtContext::Tracked(context) if matches!(**context, CtPacketContext::Invalid)
        ));
        assert_eq!(packet.mark, 0x8765_4321);
        let retry_at = now + smoltcp::time::Duration::from_millis(1);
        reservation.requeue_backpressured(packet, retry_at);
        let LocalOutputPop::Ready(packet, reservation, _) = queue.pop_ready_output(retry_at, false)
        else {
            panic!("prepared packet must survive retry")
        };
        assert!(matches!(
            &packet.ct_context,
            OutputCtContext::Tracked(context) if matches!(**context, CtPacketContext::Invalid)
        ));
        assert!(matches!(
            packet.prepared_ip,
            Some(PreparedIpProgress::Ipv6 {
                offset: 0,
                identification: 42,
                ..
            })
        ));
        assert_eq!(packet.mark, 0x8765_4321);
        drop(reservation);
    }
}
