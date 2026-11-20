use alloc::collections::{BTreeMap, VecDeque};
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;

use smoltcp::wire::{IpAddress, IpProtocol, IpVersion};

use crate::libs::rwsem::RwSem;
use crate::process::namespace::net_namespace::NetNamespace;
use crate::process::namespace::NamespaceOps;
use crate::process::ProcessState;

use super::packet::ipv6_udp_checksum;
use super::{InetSocket, RawSocket};
use crate::net::socket::utils::{IPV4_MIN_HEADER_LEN, IPV6_HEADER_LEN};

// SKB 内存计费常量 (参考 Linux 6.6 include/linux/skbuff.h)
/// SKB 数据对齐大小 (SMP_CACHE_BYTES)
const SKB_DATA_ALIGN: usize = 64;
/// SKB 管理开销 (sizeof(sk_buff) + sizeof(skb_shared_info) 对齐后)
const SKB_OVERHEAD: usize = 576;

#[derive(Debug, Default)]
pub(super) struct LoopbackRxQueue {
    pub(super) pkts: VecDeque<RawRxPacket>,
    pub(super) bytes: usize,
}

/// The ingress interface is packet metadata, not a property of the raw
/// socket's attached smoltcp interface (which can be lo for a wildcard bind).
#[derive(Debug)]
pub(super) struct RawRxPacket {
    pub(super) bytes: crate::driver::net::PacketBuffer,
    pub(super) ingress_ifindex: u32,
}

lazy_static! {
    /// 同一 netns 下的 raw socket 列表（用于 loopback 快速路径广播投递）。
    static ref RAW_SOCKET_REGISTRY: RwSem<BTreeMap<usize, Vec<Weak<RawSocket>>>> =
        RwSem::new(BTreeMap::new());
}

pub(super) fn register_raw_socket(sock: &Arc<RawSocket>) {
    let netns_id = sock.netns.ns_common().nsid.data();
    let mut reg = RAW_SOCKET_REGISTRY.write();
    let entry = reg.entry(netns_id).or_default();
    entry.push(Arc::downgrade(sock));
    // 轻量清理：避免条目无限增长。
    entry.retain(|w| w.strong_count() != 0);
}

pub(super) fn unregister_raw_socket(sock: &Arc<RawSocket>) {
    let netns_id = sock.netns.ns_common().nsid.data();
    let mut reg = RAW_SOCKET_REGISTRY.write();
    let Some(entry) = reg.get_mut(&netns_id) else {
        return;
    };
    let socket = Arc::downgrade(sock);
    entry.retain(|w| w.strong_count() != 0 && !Weak::ptr_eq(w, &socket));
    if entry.is_empty() {
        reg.remove(&netns_id);
    }
}

impl RawSocket {
    /// Match the socket identity at packet admission, without attachment locks.
    pub(crate) fn matches_ingress(
        &self,
        version: IpVersion,
        protocol: IpProtocol,
        source: IpAddress,
        destination: IpAddress,
        ingress_ifindex: u32,
    ) -> bool {
        let addresses = self.addresses.read();
        addresses.live
            && self.ip_version == version
            && self.protocol == protocol
            && addresses
                .local
                .is_none_or(|addr| addr.is_unspecified() || addr == destination)
            && addresses.remote.is_none_or(|addr| addr == source)
            && self.device_binding.allows(ingress_ifindex as usize)
    }
}

/// Snapshot candidates only when a packet actually arrives. The caller keeps
/// every upgraded reference until after the device/protocol locks drop.
pub(crate) fn snapshot_raw_ingress_sockets(
    netns: &Arc<NetNamespace>,
) -> Result<Vec<Arc<RawSocket>>, system_error::SystemError> {
    let netns_id = netns.ns_common().nsid.data();

    // Polling is a read-mostly hot path. Dead weak references are removed by
    // registration/unregistration rather than upgrading a global write lock
    // on every interface poll.
    let reg = RAW_SOCKET_REGISTRY.read();
    let entry = match reg.get(&netns_id) {
        Some(v) => v,
        None => return Ok(Vec::new()),
    };

    let mut result = Vec::new();
    result
        .try_reserve_exact(entry.len())
        .map_err(|_| system_error::SystemError::ENOMEM)?;
    for w in entry {
        if let Some(s) = w.upgrade() {
            result.push(s);
        }
    }

    Ok(result)
}

/// Loopback 投递上下文
pub(super) struct LoopbackDeliverContext<'a> {
    pub(super) packet: &'a [u8],
    pub(super) dest: IpAddress,
    pub(super) ip_version: IpVersion,
    pub(super) protocol: IpProtocol,
}

/// 检查 socket 是否应该接收该 loopback 数据包
///
/// 应用以下过滤规则：
/// 1. IP 版本和协议号匹配
/// 2. ICMP/ICMPv6 filter
/// 3. SO_BINDTODEVICE (loopback 视为 "lo")
/// 4. bind(2) 目的地址过滤
/// 5. IPV6_CHECKSUM 接收校验
fn should_deliver_to_socket(
    s: &RawSocket,
    ctx: &LoopbackDeliverContext,
    ingress_ifindex: u32,
) -> bool {
    // 1. IP 版本和协议号必须匹配
    if s.ip_version != ctx.ip_version || s.protocol != ctx.protocol {
        return false;
    }

    // 2. ICMP filter
    if ctx.protocol == IpProtocol::Icmp {
        let ihl = if ctx.packet.len() >= IPV4_MIN_HEADER_LEN {
            ((ctx.packet[0] & 0x0f) as usize) * 4
        } else {
            0
        };
        if ihl >= IPV4_MIN_HEADER_LEN && ctx.packet.len() > ihl {
            let icmp_type = ctx.packet[ihl];
            if s.options.read().icmp_filter.should_filter(icmp_type) {
                return false;
            }
        }
    }

    // 3. ICMPv6 filter
    if ctx.protocol == IpProtocol::Icmpv6 && ctx.packet.len() > IPV6_HEADER_LEN {
        let icmp_type = ctx.packet[IPV6_HEADER_LEN];
        if s.options.read().icmp6_filter.should_filter(icmp_type) {
            return false;
        }
    }

    // 4. SO_BINDTODEVICE uses the trusted ingress device, not the raw
    // socket's attached smoltcp interface.
    if !s.device_binding.allows(ingress_ifindex as usize) {
        return false;
    }

    // 5. bind(2) 目的地址过滤
    let (local, remote) = {
        let addresses = s.addresses.read();
        if !addresses.live {
            return false;
        }
        (addresses.local, addresses.remote)
    };
    if let Some(local) = local {
        if !local.is_unspecified() && local != ctx.dest {
            return false;
        }
    }
    if let Some(remote) = remote {
        if crate::net::socket::utils::extract_src_addr_from_ip_header(ctx.packet, ctx.ip_version)
            != Ok(remote)
        {
            return false;
        }
    }

    // 6. IPV6_CHECKSUM 接收校验
    if s.ip_version == IpVersion::Ipv6 && s.protocol == IpProtocol::Udp {
        let off = s.options.read().ipv6_checksum;
        if off >= 0 {
            let off = off as usize;
            if ctx.packet.len() < IPV6_HEADER_LEN {
                return false;
            }
            let payload = &ctx.packet[IPV6_HEADER_LEN..];
            if off + 2 > payload.len() {
                return false;
            }
            let got = u16::from_be_bytes([payload[off], payload[off + 1]]);
            if got == 0 {
                return false;
            }
            match ipv6_udp_checksum(ctx.packet, off) {
                Some(expect) if expect == got => {}
                _ => return false,
            }
        }
    }

    true
}

fn deliver_raw_to_sockets(
    ctx: &LoopbackDeliverContext,
    ingress_ifindex: u32,
    sockets: &[Arc<RawSocket>],
) {
    let pkt_cost = loopback_rx_mem_cost(ctx.packet.len());

    for s in sockets.iter() {
        if !should_deliver_to_socket(s, ctx, ingress_ifindex) {
            continue;
        }

        // SO_RCVBUF：投递/丢弃语义
        let rcvbuf = s.options.read().sock_rcvbuf as usize;
        let Ok(packet) = crate::driver::net::PacketBuffer::try_from_slice(ctx.packet) else {
            continue;
        };
        let enqueued = {
            let mut q = s.loopback_rx.lock();
            let can_enqueue = if q.bytes == 0 {
                // Linux/Netstack：当接收队列为空时，允许接收一个超过 rcvbuf 的 dgram
                true
            } else {
                q.bytes.saturating_add(pkt_cost) <= rcvbuf
            };
            if can_enqueue && q.pkts.try_reserve(1).is_ok() {
                q.pkts.push_back(RawRxPacket {
                    bytes: packet,
                    ingress_ifindex,
                });
                q.bytes = q.bytes.saturating_add(pkt_cost);
                true
            } else {
                false
            }
        };

        if enqueued {
            s.notify();
            let _ = s.wait_queue.wakeup(Some(ProcessState::Blocked(true)));
        }
    }
}

/// Delivery work leaves the receive callback with owned bytes and matching
/// socket references. It is executed after smoltcp and routing locks drop.
pub(crate) struct RawIngressWork {
    pub(crate) sockets: Vec<Arc<RawSocket>>,
    /// Admission-time matches occupy the prefix; the suffix is retirement only.
    pub(crate) matched: usize,
    pub(crate) packet: crate::driver::net::PacketBuffer,
    pub(crate) source: IpAddress,
    pub(crate) destination: IpAddress,
    pub(crate) protocol: IpProtocol,
    pub(crate) ingress_ifindex: u32,
    // Keep the ingress namespace pinned through deferred packet delivery.
    pub(crate) _netns: Arc<NetNamespace>,
}

impl RawIngressWork {
    pub(crate) fn execute(self) {
        // Empty bytes are retirement-only work after a failed packet copy.
        if self.packet.is_empty() {
            return;
        }
        let ctx = LoopbackDeliverContext {
            packet: &self.packet,
            dest: self.destination,
            ip_version: self.source.version(),
            protocol: self.protocol,
        };
        deliver_raw_to_sockets(&ctx, self.ingress_ifindex, &self.sockets[..self.matched]);
    }
}

/// A batch is allocated before entering interface serialization. Linking it
/// cannot fail after socket pins have been acquired by the receive callback.
#[derive(Default)]
pub(crate) struct RawIngressBatch {
    pub(crate) work: Vec<RawIngressWork>,
    next: Option<alloc::boxed::Box<RawIngressBatch>>,
    charged: usize,
}

/// Namespace-wide publication order, shared by every interface and poll path.
/// Producers publish while holding smol_iface. The sole drainer executes only
/// after leaving poll admission, so final socket/namespace retirement cannot
/// wait for the drainer's own poll guard during device teardown.
#[derive(Default)]
pub(crate) struct RawIngressQueue {
    pending: Option<alloc::boxed::Box<RawIngressBatch>>,
    draining: bool,
    charged: usize,
}

impl core::fmt::Debug for RawIngressQueue {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("RawIngressQueue")
            .field("charged", &self.charged)
            .field("draining", &self.draining)
            .finish_non_exhaustive()
    }
}

impl RawIngressQueue {
    const MAX_BYTES: usize = 4 * 1024 * 1024;

    fn charge(&mut self, bytes: usize) -> bool {
        let Some(charged) = self.charged.checked_add(bytes) else {
            return false;
        };
        if charged > Self::MAX_BYTES {
            return false;
        }
        self.charged = charged;
        true
    }

    fn release(&mut self, bytes: usize) {
        debug_assert!(self.charged >= bytes);
        self.charged -= bytes;
    }

    pub(crate) fn drain(queue: &crate::libs::spinlock::SpinLock<Self>) {
        loop {
            let pending = queue.lock_irqsave().take_pending();
            if pending.is_none() {
                return;
            }
            RawIngressBatch::execute_fifo(pending, queue);
        }
    }

    pub(crate) fn publish(&mut self, mut batch: alloc::boxed::Box<RawIngressBatch>) -> bool {
        debug_assert!(!batch.work.is_empty());
        batch.next = self.pending.take();
        self.pending = Some(batch);
        let owner = !self.draining;
        self.draining = true;
        owner
    }

    /// Taking an empty queue and relinquishing ownership are one transaction:
    /// a producer either joins this drain or becomes the next owner itself.
    pub(crate) fn take_pending(&mut self) -> Option<alloc::boxed::Box<RawIngressBatch>> {
        let pending = self.pending.take();
        if pending.is_none() {
            self.draining = false;
        }
        pending
    }
}

impl RawIngressBatch {
    pub(crate) fn reserve_header(
        &mut self,
        queue: &crate::libs::spinlock::SpinLock<RawIngressQueue>,
    ) -> bool {
        let bytes = core::mem::size_of::<Self>();
        if !queue.lock_irqsave().charge(bytes) {
            return false;
        }
        self.charged = bytes;
        true
    }

    /// All errors return the socket pins to the caller for local retirement.
    /// Charge owned allocations, not poll count: small independent polls must
    /// not exhaust admission while almost all of the byte budget remains free.
    pub(crate) fn record(
        &mut self,
        mut work: RawIngressWork,
        packet: &[u8],
        queue: &crate::libs::spinlock::SpinLock<RawIngressQueue>,
    ) -> Result<(), RawIngressWork> {
        if self.work.len() == self.work.capacity() {
            let metadata = core::mem::size_of::<RawIngressWork>();
            if !queue.lock_irqsave().charge(metadata) {
                return Err(work);
            }
            // Exact growth keeps every allocated entry charged, including a
            // spare entry left behind when the subsequent packet copy fails.
            if self.work.try_reserve_exact(1).is_err() {
                queue.lock_irqsave().release(metadata);
                return Err(work);
            }
            self.charged += metadata;
        }
        let Some(bytes) = work
            .sockets
            .capacity()
            .checked_mul(core::mem::size_of::<Arc<RawSocket>>())
            .and_then(|pins| pins.checked_add(packet.len()))
        else {
            return Err(work);
        };
        if !queue.lock_irqsave().charge(bytes) {
            return Err(work);
        }
        if work.packet.try_reserve_exact(packet.len()).is_err() {
            queue.lock_irqsave().release(bytes);
            return Err(work);
        }
        // Capacity was prepared above; no copy can allocate after admission.
        work.packet
            .extend_from_slice(packet)
            .expect("raw ingress capacity prepared");
        self.charged += bytes;
        self.work.push(work);
        Ok(())
    }

    pub(crate) fn retire(
        self: alloc::boxed::Box<Self>,
        queue: &crate::libs::spinlock::SpinLock<RawIngressQueue>,
    ) {
        let charged = self.charged;
        drop(self);
        // Building, detached, executing and empty batches retain their charge
        // until their buffers and socket/namespace pins have actually retired.
        if charged != 0 {
            queue.lock_irqsave().release(charged);
        }
    }

    /// Publication uses a stack to avoid allocation and unsafe tail pointers.
    /// Reverse detached batches outside the queue lock to restore FIFO order.
    fn execute_fifo(
        mut pending: Option<alloc::boxed::Box<Self>>,
        queue: &crate::libs::spinlock::SpinLock<RawIngressQueue>,
    ) {
        let mut ordered = None;
        while let Some(mut batch) = pending {
            pending = batch.next.take();
            batch.next = ordered;
            ordered = Some(batch);
        }
        while let Some(mut batch) = ordered {
            ordered = batch.next.take();
            for work in batch.work.drain(..) {
                work.execute();
            }
            batch.retire(queue);
            // Sustained ingress must not suppress scheduler-requested progress.
            // Some direct-poll callers may already be nonpreemptible.
            if <crate::arch::CurrentIrqArch as crate::exception::InterruptArch>::is_irq_enabled()
                && crate::process::ProcessManager::current_pcb().preempt_count() == 0
            {
                crate::sched::cond_resched();
            }
        }
    }
}

#[inline]
pub(super) fn loopback_rx_mem_cost(pkt_len: usize) -> usize {
    // Linux 的 sk_rmem_alloc 按 skb->truesize（含管理开销与对齐）计费，
    // 不是仅按 payload/IP 包长度计费。gVisor 的 RecvBufLimits 也依赖此语义：
    // 在 rcvbuf = 4 * min 时，第 4 个 min 包会因为额外开销而被丢弃。
    // 这里实现一个足够接近 Linux 6.6 行为的近似：
    // - 对齐：SKB_DATA_ALIGN(X) = ALIGN(X, SMP_CACHE_BYTES)（常见为 64）
    // - 开销：SKB_TRUESIZE(X) 还会叠加 data_align(sizeof(sk_buff)) + data_align(sizeof(skb_shared_info))
    // 由于 DragonOS 不复用 Linux 的 skb，这里用常见的合计开销近似（约 576B）。
    let aligned = (pkt_len + (SKB_DATA_ALIGN - 1)) & !(SKB_DATA_ALIGN - 1);
    aligned.saturating_add(SKB_OVERHEAD)
}
