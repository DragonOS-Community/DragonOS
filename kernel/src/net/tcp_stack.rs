//! One TCP protocol domain per network namespace.
//!
//! Devices perform IP admission and reassembly, then enqueue validated TCP.
//! Neither address ownership nor a configurable netdev owns TCP lifetimes.

use alloc::boxed::Box;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use smoltcp::iface::{Interface, SocketSet, TcpIngressHandler, TcpIngressResult};
use smoltcp::phy::PacketMeta;
use smoltcp::wire::IpRepr;

use crate::driver::net::tcp_output::{new_transport_interface, TcpOutputQueue};
use crate::driver::net::PacketBuffer;
use crate::libs::prepared_queue::{Node, Queue};
use crate::libs::{mutex::Mutex, spinlock::SpinLock};
use crate::net::nftables::{NftDeviceNames, RulesetSnapshot};
use crate::net::socket::inet::{common::port::TcpBindDomain, InetSocket};
use crate::net::tcp_close_defer::{DeferredTcpCloseRequest, TcpCloseDefer};
use crate::net::tcp_listener::TcpListenerRegistry;
use crate::process::namespace::net_namespace::NetNamespace;
use crate::process::ProcessState;

const POLL_BUDGET: usize = 64;
const MAX_INPUT_PACKETS: usize = 256;
const MAX_INPUT_BYTES: usize = 1024 * 1024;
const NO_DEADLINE: u64 = u64::MAX;

fn snapshot_required_device_names(
    namespace: &NetNamespace,
    ruleset: &RulesetSnapshot,
) -> Result<NftDeviceNames, system_error::SystemError> {
    if ruleset.requires_iface_names() {
        NftDeviceNames::snapshot(namespace)
    } else {
        Ok(NftDeviceNames::empty())
    }
}

#[derive(Debug)]
struct InputPacket {
    meta: PacketMeta,
    ip: IpRepr,
    segment: PacketBuffer,
}

#[derive(Debug, Default)]
struct InputQueue {
    events: Queue<InputEvent>,
    in_flight: usize,
    bytes: usize,
}

#[derive(Debug)]
enum InputEvent {
    Packet(InputPacket),
    Feedback(crate::net::icmp_error::IcmpErrorFeedback),
}

impl InputEvent {
    fn bytes(&self) -> usize {
        match self {
            Self::Packet(packet) => packet.segment.capacity(),
            Self::Feedback(feedback) => feedback.quote.capacity(),
        }
    }
}

// Prepared event metadata must never require a multi-page CPU allocation.
const _: () = assert!(core::mem::size_of::<Node<InputEvent>>() <= 4096);

impl InputQueue {
    fn try_push(&mut self, event: Box<Node<InputEvent>>) -> Result<(), Box<Node<InputEvent>>> {
        let bytes = event.value().bytes();
        if self.events.len() + self.in_flight >= MAX_INPUT_PACKETS
            || bytes > MAX_INPUT_BYTES.saturating_sub(self.bytes)
        {
            return Err(event);
        }
        self.bytes += bytes;
        self.events.push_node(event);
        Ok(())
    }

    fn pop(&mut self) -> Option<Box<Node<InputEvent>>> {
        let event = self.events.pop_node()?;
        self.in_flight += 1;
        Some(event)
    }

    fn requeue(&mut self, event: Box<Node<InputEvent>>) {
        self.in_flight -= 1;
        self.events.push_front_node(event);
    }

    fn complete(&mut self, event: &InputEvent) {
        self.in_flight -= 1;
        self.bytes -= event.bytes();
    }
}

pub struct TcpStack {
    namespace: Weak<NetNamespace>,
    sockets: Mutex<SocketSet<'static, 'static>>,
    context: Mutex<Interface>,
    input: SpinLock<InputQueue>,
    output: TcpOutputQueue,
    bounds: Arc<crate::net::tcp_bounds::BoundRegistry>,
    close_defer: TcpCloseDefer,
    listeners: Arc<TcpListenerRegistry>,
    pending: AtomicBool,
    poll_lock: Mutex<()>,
    deadline: AtomicU64,
    refill_requests: crate::net::tcp_bounds::RefillRequests,
}

impl core::fmt::Debug for TcpStack {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("TcpStack")
            .field("pending", &self.pending)
            .field("deadline", &self.deadline)
            .finish_non_exhaustive()
    }
}

impl TcpStack {
    pub(crate) fn queue_icmp_feedback(&self, feedback: crate::net::icmp_error::IcmpErrorFeedback) {
        self.queue_input(InputEvent::Feedback(feedback));
    }

    fn queue_input(&self, event: InputEvent) {
        let Ok(event) = Node::try_new(event) else {
            return;
        };
        // In-flight events retain both admission and their original node, so
        // blocked packets return before later feedback/data without allocation.
        // Keep rejected ownership outside the guard for unlocked retirement.
        let result = self.input.lock().try_push(event);
        if result.is_ok() {
            self.request_poll();
        }
    }

    fn process_icmp_feedback(
        &self,
        namespace: &Arc<NetNamespace>,
        feedback: &crate::net::icmp_error::IcmpErrorFeedback,
    ) {
        // TCP quotations share this event with RAW error subscribers. No
        // device, FIB, input or protocol lock is held during callbacks.
        crate::net::socket::inet::raw::handle_icmp_feedback(namespace, feedback);
        let Some(seq) = feedback.seq else {
            return;
        };
        let Some(mtu) = feedback.mtu() else {
            // Pins are obtained in bounded batches without holding the
            // registry lock. Owners acquire inner -> SocketSet and recheck
            // the quote, preserving connect/accept/close lifetime rules.
            let end = self.bounds.end();
            for start in (0..end).step_by(crate::net::tcp_bounds::BOUND_BATCH) {
                let pins = self.bounds.snapshot(start, end);
                for owner in pins.iter().flatten() {
                    owner.handle_tcp_icmp(feedback);
                }
            }
            return;
        };
        let (source, destination) = crate::net::icmp_error::routed_quote_flow(namespace, feedback);
        let router = namespace.router();
        let routes = crate::net::route::lock_output_routes(&router, namespace.device_list());
        let mut sockets = self.sockets.lock();
        for (_, socket) in sockets.iter_mut() {
            let smoltcp::socket::Socket::Tcp(socket) = socket else {
                continue;
            };
            let local = smoltcp::wire::IpEndpoint::new(feedback.source, feedback.src_port);
            let remote = smoltcp::wire::IpEndpoint::new(feedback.destination, feedback.dst_port);
            if !socket.accepts_pmtu_quote(
                local,
                remote,
                smoltcp::wire::TcpSeqNumber(seq),
                feedback.ingress_ifindex,
            ) {
                continue;
            }
            if socket.is_passive_open() {
                // Linux handles SYNACK requests before the PMTU path and
                // never learns a path or retransmits data for that child.
                continue;
            }
            if socket.pmtu_discover() >= 4 {
                continue;
            }
            if matches!(feedback.destination, smoltcp::wire::IpAddress::Ipv6(_))
                && (mtu < 1280
                    || socket
                        .output_ip_mtu()
                        .is_some_and(|old| mtu as usize >= old))
            {
                continue;
            }
            let required_oif = socket.bound_device();
            let Some(route) = routes.lookup(destination, required_oif.map(|index| index.get()))
            else {
                continue;
            };
            let learned =
                crate::net::route::pmtu::learn_on_route(&router, route, source, destination, mtu);
            // Cache sharing and this socket's PMTU cookie are separate:
            // an unchanged exception can still require this TCP to shrink.
            socket.schedule_pmtu_retransmit(learned.path);
            break;
        }
    }

    pub(crate) fn new(
        namespace: Weak<NetNamespace>,
        bounds: Arc<crate::net::tcp_bounds::BoundRegistry>,
    ) -> Self {
        let listeners = Arc::new(TcpListenerRegistry::new());
        let mut sockets = SocketSet::new(Vec::new());
        sockets.set_tcp_listen_registry(Some(listeners.clone()));
        Self {
            namespace,
            sockets: Mutex::new(sockets),
            context: Mutex::new(new_transport_interface()),
            input: SpinLock::new(InputQueue::default()),
            output: TcpOutputQueue::new(),
            bounds,
            close_defer: TcpCloseDefer::new(),
            listeners,
            pending: AtomicBool::new(false),
            poll_lock: Mutex::new(()),
            deadline: AtomicU64::new(NO_DEADLINE),
            refill_requests: crate::net::tcp_bounds::RefillRequests::new(),
        }
    }

    pub fn sockets(&self) -> &Mutex<SocketSet<'static, 'static>> {
        &self.sockets
    }

    /// Prepare shared protocol-table storage in task context. The table keeps
    /// the allocating creator's receipt until that actual array is retired;
    /// individual socket close does not refund retained table capacity.
    pub(crate) fn prepare_socket_capacity(
        &self,
        creator: &Arc<crate::cgroup::memory::object::ObjectAccount>,
    ) -> Result<(), crate::mm::allocation::MemoryAllocationError> {
        loop {
            let slots = {
                let sockets = self.sockets.lock();
                if sockets.available_slots() != 0 {
                    return Ok(());
                }
                sockets.storage_slots()
            };
            let capacity = slots.checked_mul(2).filter(|n| *n > slots).unwrap_or(4);
            if capacity <= slots {
                return Err(system_error::SystemError::EOVERFLOW.into());
            }
            // Admission and the backing allocator never run under SocketSet.
            let prepared = crate::net::socket::buffer::try_socket_set_storage(creator, capacity);
            let mut sockets = self.sockets.lock();
            if sockets.available_slots() != 0 {
                drop(sockets);
                drop(prepared);
                return Ok(());
            }
            let prepared = match prepared {
                Ok(prepared) => prepared,
                Err(error) => {
                    drop(sockets);
                    return Err(error);
                }
            };
            if sockets.storage_slots() > capacity {
                drop(sockets);
                drop(prepared);
                continue;
            }
            let retired = sockets
                .replace_prepared_storage(prepared)
                .unwrap_or_else(|_| {
                    unreachable!("prepared empty SocketSet array covers live slots")
                });
            drop(sockets);
            drop(retired);
            return Ok(());
        }
    }

    pub fn smol_iface(&self) -> &Mutex<Interface> {
        &self.context
    }

    pub fn net_namespace(&self) -> Option<Arc<NetNamespace>> {
        self.namespace.upgrade()
    }

    pub(crate) fn reserve_bound_slot(
        self: &Arc<Self>,
        creator: &Arc<crate::cgroup::memory::object::ObjectAccount>,
    ) -> Result<
        crate::net::tcp_bounds::BoundSlotReservation,
        crate::mm::allocation::MemoryAllocationError,
    > {
        self.bounds.reserve(creator)
    }

    pub(crate) fn publish_bound_slot(
        &self,
        index: usize,
        id: u64,
        socket: Arc<dyn InetSocket>,
    ) -> bool {
        self.bounds.publish(index, id, socket)
    }

    pub(crate) fn unpublish_bound_slot(&self, index: usize, id: u64) {
        self.bounds.unpublish(index, id);
    }

    pub(crate) fn cancel_bound_slot(&self, index: usize, id: u64) {
        self.bounds.cancel(index, id);
    }

    pub fn unbind_socket(&self, socket: Arc<dyn InetSocket>) {
        self.bounds.unbind(&socket);
    }

    pub fn notify_all_bound_sockets(&self) {
        let end = self.bounds.end();
        for start in (0..end).step_by(crate::net::tcp_bounds::BOUND_BATCH) {
            let pins = self.bounds.snapshot(start, end);
            for socket in pins.iter().flatten() {
                socket.notify();
                socket
                    .wait_queue()
                    .wakeup(Some(ProcessState::Blocked(true)));
            }
        }
    }

    pub(crate) fn refill_listeners(&self) {
        let Some(batch) = self.refill_requests.next_batch(self.bounds.end()) else {
            return;
        };
        let pins = self.bounds.snapshot(batch.start, batch.end);
        for socket in pins.iter().flatten() {
            socket.refill_tcp_listener();
        }
        // This method is called before poll serialization, not from an IRQ.
        drop(pins);
        self.refill_requests.finish_batch(batch);
    }

    pub(crate) fn request_listener_refill(&self) {
        self.refill_requests.request();
        if let Some(namespace) = self.namespace.upgrade() {
            namespace.notify_deadline_changed();
        }
    }

    pub fn register_tcp_listener(&self, id: u64, domain: TcpBindDomain, port: u16, device: u32) {
        self.listeners.register(id, domain, port, device);
    }

    pub fn unregister_tcp_listener(&self, id: u64) {
        self.listeners.unregister(id);
    }

    pub fn defer_tcp_close(&self, request: DeferredTcpCloseRequest) {
        self.close_defer
            .defer_tcp_close(crate::time::Instant::now().into(), request);
        self.request_poll();
    }

    /// Publishing work is independent of interface NAPI eligibility (e.g. lo
    /// being administratively DOWN cannot stop another device's TCP timers).
    pub(crate) fn request_poll(&self) {
        self.pending.store(true, Ordering::Release);
        if let Some(namespace) = self.namespace.upgrade() {
            namespace.notify_deadline_changed();
        }
    }

    pub(crate) fn poll_due(&self, now_us: u64) -> bool {
        self.pending.load(Ordering::Acquire)
            || self.refill_requests.pending()
            || self.deadline.load(Ordering::Acquire) <= now_us
    }

    pub(crate) fn deadline_us(&self) -> Option<u64> {
        if self.pending.load(Ordering::Acquire) || self.refill_requests.pending() {
            return Some(0);
        }
        let deadline = self.deadline.load(Ordering::Acquire);
        (deadline != NO_DEADLINE).then_some(deadline)
    }

    /// Progress one bounded input batch and one ordinary smoltcp output pass.
    /// A caller that queued data must not mistake another poller's ownership
    /// for an idle stack: the next poll pass must run after that owner exits.
    /// No upper-layer notification runs under TCP locks.
    pub fn poll(&self) -> bool {
        let poll_guard = self.poll_lock.lock();
        let Some(namespace) = self.namespace.upgrade() else {
            return false;
        };
        self.pending.store(false, Ordering::Release);
        // One immutable policy/name view per bounded TCP poll. Snapshot names
        // before FIB or protocol locks, and never substitute empty names if
        // allocation fails while a rule requires them.
        let ruleset = namespace.nftables().snapshot();
        let device_names = snapshot_required_device_names(&namespace, &ruleset);
        let mut snapshot_failed = device_names.is_err();
        let needs_masquerade_addresses = [
            smoltcp::wire::IpVersion::Ipv4,
            smoltcp::wire::IpVersion::Ipv6,
        ]
        .into_iter()
        .any(|version| {
            ruleset.requires_masquerade(version, crate::net::nftables::NftIpv4Hook::PostRouting)
        });
        let mut input_blocked = false;
        let mut feedback_count = 0;
        for _ in 0..POLL_BUDGET {
            let Ok(device_names) = device_names.as_ref() else {
                break;
            };
            let event = {
                let mut input = self.input.lock();
                // Keep the bounded control budget without allowing later data
                // to overtake a feedback event left at the front.
                if feedback_count == POLL_BUDGET / 2
                    && matches!(input.events.front(), Some(InputEvent::Feedback(_)))
                {
                    break;
                }
                let Some(event) = input.pop() else {
                    break;
                };
                event
            };
            let packet = match event.value() {
                InputEvent::Feedback(feedback) => {
                    feedback_count += 1;
                    self.process_icmp_feedback(&namespace, feedback);
                    let mut input = self.input.lock();
                    input.complete(event.value());
                    continue;
                }
                InputEvent::Packet(packet) => packet,
            };
            // IP admission and TCP validation already happened on the device.
            // Reserve listener capacity outside protocol/router locks, using
            // the same inner -> SocketSet order as listen/accept/shutdown.
            if let Ok(tcp) = smoltcp::wire::TcpPacket::new_checked(&packet.segment[..]) {
                if tcp.syn() && !tcp.ack() && !tcp.rst() {
                    let local =
                        smoltcp::wire::IpEndpoint::new(packet.ip.dst_addr(), tcp.dst_port());
                    // Keep admission and dispatch serialized: another poller
                    // must not consume the last Listen slot after preparation
                    // but before this SYN is matched. Refill is task-context
                    // TryOnly and takes no reclaim/OOM path; all preparation
                    // and retirement remain outside inner/SocketSet/FIB locks.
                    let end = self.bounds.end();
                    for start in (0..end).step_by(crate::net::tcp_bounds::BOUND_BATCH) {
                        let pins = self.bounds.snapshot(start, end);
                        for socket in pins.iter().flatten() {
                            socket.prepare_tcp_syn(local, packet.meta.id);
                        }
                    }
                }
            }
            let router = namespace.router();
            let mut routes =
                crate::net::route::lock_output_routes(&router, namespace.device_list());
            if needs_masquerade_addresses && routes.prepare_masquerade_addresses().is_err() {
                drop(routes);
                let mut input = self.input.lock();
                input.requeue(event);
                input_blocked = true;
                snapshot_failed = true;
                break;
            }
            let mut sockets = self.sockets.lock();
            let mut context = self.context.lock();
            let now: smoltcp::time::Instant = crate::time::Instant::now().into();
            let mut device = self
                .output
                .device(&namespace, &routes, &ruleset, device_names);
            if !context.process_tcp_ingress(
                now,
                &mut device,
                &mut sockets,
                packet.meta,
                packet.ip.clone(),
                &packet.segment,
            ) {
                let mut input = self.input.lock();
                input.requeue(event);
                input_blocked = true;
                break;
            }
            let mut input = self.input.lock();
            input.complete(event.value());
        }
        let mut poll_at = match &device_names {
            Ok(device_names) => {
                let router = namespace.router();
                let mut routes =
                    crate::net::route::lock_output_routes(&router, namespace.device_list());
                if needs_masquerade_addresses && routes.prepare_masquerade_addresses().is_err() {
                    snapshot_failed = true;
                    None
                } else {
                    let mut sockets = self.sockets.lock();
                    let mut context = self.context.lock();
                    let now: smoltcp::time::Instant = crate::time::Instant::now().into();
                    let mut device =
                        self.output
                            .device(&namespace, &routes, &ruleset, device_names);
                    context.poll_egress(now, &mut device, &mut sockets);
                    let close_deadline = self.close_defer.reap_closed(now, &mut sockets);
                    match (context.poll_at(now, &sockets), close_deadline) {
                        (Some(protocol), Some(close)) => Some(core::cmp::min(protocol, close)),
                        (protocol, close) => protocol.or(close),
                    }
                }
            }
            Err(_) => None,
        };
        // A changed OUTPUT/POST ruleset may have stopped egress (or refused
        // the token for an immediate ACK) before advancing socket state.
        // All FIB and smoltcp guards above have been released; restart once
        // with a fresh policy/name/MASQUERADE view instead of waiting for an
        // unrelated TCP timer or another incoming segment.
        let policy_changed = ruleset.generation != namespace.nftables().generation();
        // Output queue exhaustion is relieved by this round's drain. If no
        // output was admitted, allocation pressure needs a bounded retry, not
        // a busy loop over an unchanged input packet.
        if input_blocked || snapshot_failed {
            let retry_now: smoltcp::time::Instant = crate::time::Instant::now().into();
            let retry_at = retry_now + smoltcp::time::Duration::from_millis(1);
            poll_at = Some(poll_at.map_or(retry_at, |at| at.min(retry_at)));
        }
        let now: smoltcp::time::Instant = crate::time::Instant::now().into();
        let mut immediate = policy_changed
            || !input_blocked
                && !snapshot_failed
                && (poll_at.is_some_and(|at| at <= now) || {
                    let input = self.input.lock();
                    !input.events.is_empty()
                });
        let output = self.output.drain(&namespace, POLL_BUDGET);
        immediate |= output.immediate;
        if let Some(retry_at) = output.retry_at {
            poll_at = Some(poll_at.map_or(retry_at, |at| at.min(retry_at)));
        }
        let deadline = poll_at.map_or(NO_DEADLINE, |at| at.total_micros().max(0) as u64);
        let previous_deadline = self.deadline.swap(deadline, Ordering::AcqRel);
        // Notification may flush a socket's TCP cork and call poll() again.
        // The protocol pass is complete, so release serialization first.
        drop(poll_guard);
        self.notify_all_bound_sockets();
        if immediate {
            self.request_poll();
        } else if previous_deadline != deadline {
            namespace.notify_deadline_changed();
        }
        immediate || self.pending.load(Ordering::Acquire)
    }
}

impl TcpIngressHandler for TcpStack {
    fn handle_tcp_ingress(
        &self,
        meta: PacketMeta,
        ip: &IpRepr,
        segment: &[u8],
    ) -> TcpIngressResult {
        if segment.len() > MAX_INPUT_BYTES {
            return TcpIngressResult::Consumed;
        }
        let Ok(owned) = PacketBuffer::try_from_slice(segment) else {
            return TcpIngressResult::Consumed;
        };
        self.queue_input(InputEvent::Packet(InputPacket {
            meta,
            ip: ip.clone(),
            segment: owned,
        }));
        TcpIngressResult::Consumed
    }
}

#[cfg(test)]
mod input_queue_tests {
    use super::*;
    use smoltcp::wire::{IpProtocol, Ipv4Address, Ipv4Repr};

    fn packet(bytes: usize) -> Box<Node<InputEvent>> {
        Node::try_new(InputEvent::Packet(InputPacket {
            meta: PacketMeta::default(),
            ip: IpRepr::Ipv4(Ipv4Repr {
                src_addr: Ipv4Address::new(192, 0, 2, 1),
                dst_addr: Ipv4Address::new(192, 0, 2, 2),
                next_header: IpProtocol::Tcp,
                payload_len: bytes,
                hop_limit: 64,
            }),
            segment: PacketBuffer::try_with_capacity(bytes).unwrap(),
        }))
        .unwrap()
    }

    #[test]
    fn in_flight_event_retains_count_and_bytes_until_complete() {
        let mut input = InputQueue::default();
        for _ in 0..MAX_INPUT_PACKETS {
            input.try_push(packet(64)).unwrap();
        }
        let event = input.pop().unwrap();
        let address = event.as_ref() as *const Node<InputEvent>;
        assert_eq!(input.in_flight, 1);
        assert_eq!(input.bytes, MAX_INPUT_PACKETS * 64);
        assert!(input.try_push(packet(64)).is_err());
        input.requeue(event);
        assert_eq!(input.in_flight, 0);
        let event = input.pop().unwrap();
        assert_eq!(event.as_ref() as *const Node<InputEvent>, address);
        input.complete(event.value());
        drop(event);
        assert_eq!(input.bytes, (MAX_INPUT_PACKETS - 1) * 64);
        input.try_push(packet(64)).unwrap();
        while let Some(event) = input.pop() {
            input.complete(event.value());
        }
        assert_eq!(input.in_flight, 0);
        assert_eq!(input.bytes, 0);
        assert!(input.events.is_empty());
    }

    #[test]
    fn blocked_packet_stays_before_feedback_and_later_packet() {
        use crate::net::icmp_error::{IcmpErrorFeedback, IcmpErrorKind};
        use smoltcp::wire::IpAddress;
        let mut input = InputQueue::default();
        input.try_push(packet(64)).unwrap();
        let blocked = input.pop().unwrap();
        let feedback = IcmpErrorFeedback {
            source: IpAddress::v4(192, 0, 2, 1),
            destination: IpAddress::v4(192, 0, 2, 2),
            protocol: IpProtocol::Tcp,
            src_port: 1,
            dst_port: 2,
            seq: Some(1),
            kind: IcmpErrorKind::Mtu(1280),
            icmp_type: 3,
            icmp_code: 4,
            offender: IpAddress::v4(192, 0, 2, 3),
            ingress_ifindex: 2,
            quote: PacketBuffer::try_with_capacity(128).unwrap(),
            transport_offset: 20,
            outer_ttl: 64,
            outer_tos: 0,
        };
        input
            .try_push(Node::try_new(InputEvent::Feedback(feedback)).unwrap())
            .unwrap();
        input.try_push(packet(64)).unwrap();
        input.requeue(blocked);
        for feedback_expected in [false, true, false] {
            let event = input.pop().unwrap();
            assert_eq!(
                matches!(event.value(), InputEvent::Feedback(_)),
                feedback_expected
            );
            input.complete(event.value());
        }
        assert_eq!(input.bytes, 0);
        assert_eq!(input.in_flight, 0);
        assert!(input.events.is_empty());
    }

    #[test]
    fn byte_limit_survives_block_and_resumes_after_completion() {
        let mut input = InputQueue::default();
        input.try_push(packet(MAX_INPUT_BYTES)).unwrap();
        let event = input.pop().unwrap();
        assert!(input.try_push(packet(1)).is_err());
        input.requeue(event);
        assert_eq!(input.bytes, MAX_INPUT_BYTES);
        let event = input.pop().unwrap();
        input.complete(event.value());
        drop(event);
        input.try_push(packet(1)).unwrap();
        assert_eq!(input.bytes, 1);
    }
}
