//! One TCP protocol domain per network namespace.
//!
//! Devices perform IP admission and reassembly, then enqueue validated TCP.
//! Neither address ownership nor a configurable netdev owns TCP lifetimes.

use alloc::collections::VecDeque;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use smoltcp::iface::{Interface, SocketSet, TcpIngressHandler, TcpIngressResult};
use smoltcp::phy::PacketMeta;
use smoltcp::wire::IpRepr;

use crate::driver::net::tcp_output::{new_transport_interface, TcpOutputQueue};
use crate::libs::{mutex::Mutex, rwlock::RwLock, spinlock::SpinLock};
use crate::net::socket::inet::{common::port::TcpBindDomain, InetSocket};
use crate::net::tcp_close_defer::{DeferredTcpCloseRequest, TcpCloseDefer};
use crate::net::tcp_listener::TcpListenerRegistry;
use crate::process::namespace::net_namespace::NetNamespace;
use crate::process::ProcessState;

const POLL_BUDGET: usize = 64;
const MAX_INPUT_PACKETS: usize = 256;
const MAX_INPUT_BYTES: usize = 1024 * 1024;
const NO_DEADLINE: u64 = u64::MAX;

#[derive(Debug)]
struct InputPacket {
    meta: PacketMeta,
    ip: IpRepr,
    segment: Vec<u8>,
}

#[derive(Debug, Default)]
struct InputQueue {
    packets: VecDeque<InputPacket>,
    in_flight: usize,
    bytes: usize,
}

pub struct TcpStack {
    namespace: Weak<NetNamespace>,
    sockets: Mutex<SocketSet<'static>>,
    context: Mutex<Interface>,
    input: SpinLock<InputQueue>,
    output: TcpOutputQueue,
    bounds: RwLock<Arc<Vec<Arc<dyn InetSocket>>>>,
    close_defer: TcpCloseDefer,
    listeners: Arc<TcpListenerRegistry>,
    pending: AtomicBool,
    poll_lock: Mutex<()>,
    deadline: AtomicU64,
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
    pub(crate) fn new(namespace: Weak<NetNamespace>) -> Self {
        let listeners = Arc::new(TcpListenerRegistry::new());
        let mut sockets = SocketSet::new(Vec::new());
        sockets.set_tcp_listen_registry(Some(listeners.clone()));
        Self {
            namespace,
            sockets: Mutex::new(sockets),
            context: Mutex::new(new_transport_interface()),
            input: SpinLock::new(InputQueue::default()),
            output: TcpOutputQueue::new(),
            bounds: RwLock::new(Arc::new(Vec::new())),
            close_defer: TcpCloseDefer::new(),
            listeners,
            pending: AtomicBool::new(false),
            poll_lock: Mutex::new(()),
            deadline: AtomicU64::new(NO_DEADLINE),
        }
    }

    pub fn sockets(&self) -> &Mutex<SocketSet<'static>> {
        &self.sockets
    }

    pub fn smol_iface(&self) -> &Mutex<Interface> {
        &self.context
    }

    pub fn net_namespace(&self) -> Option<Arc<NetNamespace>> {
        self.namespace.upgrade()
    }

    pub fn bind_socket(&self, socket: Arc<dyn InetSocket>) {
        let mut bounds = self.bounds.write();
        if !bounds.iter().any(|bound| Arc::ptr_eq(bound, &socket)) {
            Arc::make_mut(&mut *bounds).push(socket);
        }
    }

    pub fn unbind_socket(&self, socket: Arc<dyn InetSocket>) {
        let mut bounds = self.bounds.write();
        if let Some(index) = bounds.iter().position(|bound| Arc::ptr_eq(bound, &socket)) {
            Arc::make_mut(&mut *bounds).remove(index);
        }
    }

    pub fn notify_all_bound_sockets(&self) {
        let bounds = self.bounds.read_irqsave().clone();
        for socket in bounds.iter() {
            socket.notify();
            socket
                .wait_queue()
                .wakeup(Some(ProcessState::Blocked(true)));
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
        self.pending.load(Ordering::Acquire) || self.deadline.load(Ordering::Acquire) <= now_us
    }

    pub(crate) fn deadline_us(&self) -> Option<u64> {
        if self.pending.load(Ordering::Acquire) {
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
        let mut input_blocked = false;
        for _ in 0..POLL_BUDGET {
            let packet = {
                let mut input = self.input.lock();
                let Some(packet) = input.packets.pop_front() else {
                    break;
                };
                input.in_flight += 1;
                packet
            };
            // IP admission and TCP validation already happened on the device.
            // Reserve listener capacity outside protocol/router locks, using
            // the same inner -> SocketSet order as listen/accept/shutdown.
            if let Ok(tcp) = smoltcp::wire::TcpPacket::new_checked(&packet.segment[..]) {
                if tcp.syn() && !tcp.ack() && !tcp.rst() {
                    let bounds = self.bounds.read_irqsave().clone();
                    let local =
                        smoltcp::wire::IpEndpoint::new(packet.ip.dst_addr(), tcp.dst_port());
                    for socket in bounds.iter() {
                        socket.prepare_tcp_syn(local, packet.meta.id);
                    }
                }
            }
            let router = namespace.router();
            let routes = crate::net::route::lock_output_routes(&router, namespace.device_list());
            let mut sockets = self.sockets.lock();
            let mut context = self.context.lock();
            let now: smoltcp::time::Instant = crate::time::Instant::now().into();
            let mut device = self.output.device(&routes);
            if !context.process_tcp_ingress(
                now,
                &mut device,
                &mut sockets,
                packet.meta,
                packet.ip.clone(),
                &packet.segment,
            ) {
                let mut input = self.input.lock();
                input.in_flight -= 1;
                input.packets.push_front(packet);
                input_blocked = true;
                break;
            }
            let mut input = self.input.lock();
            input.in_flight -= 1;
            input.bytes -= packet.segment.capacity();
        }
        let router = namespace.router();
        let routes = crate::net::route::lock_output_routes(&router, namespace.device_list());
        let mut sockets = self.sockets.lock();
        let mut context = self.context.lock();
        let now: smoltcp::time::Instant = crate::time::Instant::now().into();
        let mut device = self.output.device(&routes);
        context.poll_egress(now, &mut device, &mut sockets);
        let close_deadline = self.close_defer.reap_closed(now, &mut sockets);
        let mut poll_at = match (context.poll_at(now, &sockets), close_deadline) {
            (Some(protocol), Some(close)) => Some(core::cmp::min(protocol, close)),
            (protocol, close) => protocol.or(close),
        };
        // Output queue exhaustion is relieved by this round's drain. If no
        // output was admitted, allocation pressure needs a bounded retry, not
        // a busy loop over an unchanged input packet.
        if input_blocked {
            poll_at = Some(now + smoltcp::time::Duration::from_millis(1));
        }
        let immediate = !input_blocked
            && (poll_at.is_some_and(|at| at <= now) || !self.input.lock().packets.is_empty());
        let deadline = poll_at.map_or(NO_DEADLINE, |at| at.total_micros().max(0) as u64);
        let previous_deadline = self.deadline.swap(deadline, Ordering::AcqRel);
        drop(context);
        drop(sockets);
        drop(routes);
        drop(router);
        let output_pending = self.output.drain(&namespace, POLL_BUDGET);
        // Notification may flush a socket's TCP cork and call poll() again.
        // The protocol pass is complete, so release serialization first.
        drop(poll_guard);
        self.notify_all_bound_sockets();
        if immediate || output_pending {
            self.request_poll();
        } else if previous_deadline != deadline {
            namespace.notify_deadline_changed();
        }
        immediate || output_pending || self.pending.load(Ordering::Acquire)
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
        let mut owned = Vec::new();
        if owned.try_reserve_exact(segment.len()).is_err() {
            return TcpIngressResult::Consumed;
        }
        owned.extend_from_slice(segment);
        {
            let mut input = self.input.lock();
            if input.packets.len() + input.in_flight >= MAX_INPUT_PACKETS
                || input.bytes.saturating_add(owned.capacity()) > MAX_INPUT_BYTES
                || input.packets.try_reserve(1).is_err()
            {
                return TcpIngressResult::Consumed;
            }
            input.bytes += owned.capacity();
            input.packets.push_back(InputPacket {
                meta,
                ip: ip.clone(),
                segment: owned,
            });
        }
        self.request_poll();
        TcpIngressResult::Consumed
    }
}
