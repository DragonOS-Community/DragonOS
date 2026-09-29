use crate::driver::net::bridge::BridgeDriver;
use crate::driver::net::loopback::LoopbackInterface;
use crate::driver::net::IfacePollScope;
use crate::exception::workqueue::{Work, WorkQueue};
use crate::init::initcall::INITCALL_SUBSYS;
use crate::libs::mutex::Mutex;
use crate::libs::rwlock::{RwLock, RwLockReadGuard, RwLockWriteGuard};
use crate::libs::rwsem::{RwSem, RwSemReadGuard, RwSemWriteGuard};
use crate::libs::spinlock::SpinLock;
use crate::libs::wait_queue::WaitQueue;
use crate::net::conntrack::{CtCandidate, CtConfirm, CtError, CtPacketContext, CtState};
use crate::net::ipv4_defrag::{DefragDomain, Ipv4Defragmenter, ReassembledIpv4, ReassemblyResult};
use crate::net::ipv6_defrag::{
    DefragDomain as Ipv6DefragDomain, Ipv6Defragmenter, ReassembledIpv6,
    ReassemblyResult as Ipv6ReassemblyResult,
};
use crate::net::neighbor::NeighborTable;
use crate::net::nftables::NftNamespaceState;
use crate::net::routing::Router;
use crate::net::socket::inet::datagram::udp_bindings::UdpBindingTable;
use crate::net::socket::netlink::table::{
    generate_supported_netlink_kernel_sockets, NetlinkKernelSocket, NetlinkSocketTable,
};
use crate::net::socket::packet::{
    membership_value, FanoutGroup, FanoutJoinParams, PacketIngressMetadata, PacketSocket,
};
use crate::net::socket::unix::ns::UnixAbstractTable;
use crate::process::fork::CloneFlags;
use crate::process::kthread::{KernelThreadClosure, KernelThreadMechanism};
use crate::process::namespace::{nsproxy::NsProxy, NamespaceOps, NamespaceType};
use crate::process::ProcessControlBlock;
use crate::process::ProcessManager;
use crate::rcu::{RcuArcSlot, RcuOptionArcSlot};
use crate::time::{Duration, Instant};
use crate::{
    driver::net::napi::{napi_is_disabled, napi_schedule, NapiScheduleResult},
    driver::net::Iface,
    process::namespace::{nsproxy::NsCommon, user_namespace::UserNamespace},
};
use alloc::boxed::Box;
use alloc::collections::{BTreeMap, VecDeque};
use alloc::string::{String, ToString};
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use core::sync::atomic::{AtomicI32, AtomicU16, AtomicU32};
use hashbrown::HashMap;
use ida::IdAllocator;
use net_poll_state::DueResult;
use smoltcp::wire::{EthernetAddress, HardwareAddress};
use system_error::SystemError;
use unified_init::macros::unified_init;

lazy_static! {
    /// # 所有网络设备，进程，socket的初始网络命名空间
    pub static ref INIT_NET_NAMESPACE: Arc<NetNamespace> = NetNamespace::new_root();
    /// Netns teardown may wait for NAPI and must not block unrelated system work.
    static ref NETNS_TEARDOWN_WQ: Arc<WorkQueue> = WorkQueue::new("netns_cleanup");
}

/// # 网络命名空间计数器
/// 用于生成唯一的网络命名空间ID
/// 每次创建新的网络命名空间时，都会增加这个计数器
pub static mut NETNS_COUNTER: AtomicUsize = AtomicUsize::new(0);

const PACKET_SOCKET_CLEANUP_RETRY_MIN: Duration = Duration::from_millis(100);
const PACKET_SOCKET_CLEANUP_RETRY_MAX: Duration = Duration::from_secs(5);

type NetnsDeviceMap = BTreeMap<usize, Arc<dyn Iface>>;

/// A candidate device map and ingress bindings for a route-free software
/// device. Used when a new veth is published or a veth enters another netns.
pub(crate) struct PreparedNetnsDeviceAddition {
    netns: Arc<NetNamespace>,
    candidate: NetnsDeviceMap,
    bindings: Vec<(Arc<dyn Iface>, crate::driver::net::PreparedNetnsBinding)>,
}

impl PreparedNetnsDeviceAddition {
    pub(crate) fn publish(self) {
        let mut devices = self.netns.device_list_mut();
        let added_broadcasts: usize = self
            .bindings
            .iter()
            .map(|(iface, _)| iface.common().configured_ipv4_broadcast_count())
            .sum();
        for (iface, binding) in self.bindings {
            binding.publish(iface.common());
        }
        *devices = self.candidate;
        self.netns
            .publish_ipv4_broadcast_count_change(0, added_broadcasts);
        drop(devices);
        self.netns.notify_deadline_changed();
    }
}

/// RTNL keeps the device map stable after preparation. The FIB candidate and
/// all projection allocations are ready before a caller shuts down a device.
pub(crate) struct PreparedNetnsDeviceRemoval<'rtnl> {
    rtnl: &'rtnl crate::net::rtnl::RtnlGuard,
    netns: Arc<NetNamespace>,
    ifindices: Vec<usize>,
    routes: crate::net::route::PreparedIfaceUnregister<'rtnl>,
}

impl PreparedNetnsDeviceRemoval<'_> {
    pub(crate) fn publish(self) {
        let mut devices = self.netns.device_list_mut();
        let removed_broadcasts: usize = self
            .ifindices
            .iter()
            .map(|ifindex| {
                devices
                    .get(ifindex)
                    .expect("RTNL keeps prepared devices registered")
                    .common()
                    .configured_ipv4_broadcast_count()
            })
            .sum();
        self.routes.publish(&self.netns, || {
            for ifindex in &self.ifindices {
                devices
                    .remove(ifindex)
                    .expect("RTNL keeps prepared devices registered");
            }
        });
        self.netns
            .publish_ipv4_broadcast_count_change(removed_broadcasts, 0);
        drop(devices);
        self.netns
            .finish_removed_devices_locked(self.rtnl, &self.ifindices);
    }
}

#[derive(Debug)]
struct NetnsTeardownWork {
    payload: Arc<SpinLock<Option<NetnsTeardownPayload>>>,
    work: Arc<Work>,
}

#[derive(Debug)]
struct NetnsTeardownPayload {
    devices: NetnsDeviceMap,
    nftables: Box<NftNamespaceState>,
    conntrack: CtState,
}

impl NetnsTeardownWork {
    fn new() -> Self {
        let payload = Arc::new(SpinLock::new(None::<NetnsTeardownPayload>));
        let worker_payload = payload.clone();
        let work = Work::new(move || {
            let mut payload = worker_payload
                .lock()
                .take()
                .expect("queued netns teardown work must own its payload");
            teardown_netns_devices(&mut payload.devices);
            drop(payload.nftables);
            drop(payload.conntrack);
        });
        Self { payload, work }
    }

    fn enqueue(&self, payload: NetnsTeardownPayload) {
        let mut slot = self.payload.lock();
        debug_assert!(slot.is_none());
        *slot = Some(payload);
        drop(slot);
        NETNS_TEARDOWN_WQ.enqueue(self.work.clone());
    }
}

fn teardown_netns_devices(devices: &mut NetnsDeviceMap) {
    // Drop can run on the last NAPI owner, so all blocking work stays here in
    // process context. Sever bridge/veth edges first; a peer in another live
    // namespace must be unregistered with its own FIB and sysfs projection.
    while let Some((ifindex, iface)) = devices.first_key_value() {
        let ifindex = *ifindex;
        let iface = iface.clone();
        let rtnl = crate::net::rtnl::lock();
        crate::net::link::topology::teardown_detached_netns_device(&rtnl, iface.clone());
        drop(rtnl);
        iface.common().close_tx_and_wait();
        iface.begin_admin_down();
        if let Some(napi) = iface.napi_struct() {
            crate::driver::net::napi::napi_pause_and_wait(&napi);
            iface.quiesce_admin_down();
            crate::driver::net::napi::napi_disable(&napi);
        } else {
            iface.quiesce_admin_down();
        }
        iface.common().retire_for_device_removal();
        iface.clear_net_state(crate::driver::net::NetDeivceState::__LINK_STATE_PRESENT);
        crate::driver::net::netdev_unregister_kobject(iface.clone());
        iface.clear_net_namespace();
        devices.remove(&ifindex);
    }
}

fn try_snapshot_devices(
    devices: &BTreeMap<usize, Arc<dyn Iface>>,
    additional: Option<&Arc<dyn Iface>>,
) -> Result<Vec<Arc<dyn Iface>>, SystemError> {
    let count = devices
        .len()
        .checked_add(usize::from(additional.is_some()))
        .ok_or(SystemError::ENOMEM)?;
    let mut participants = Vec::new();
    participants
        .try_reserve_exact(count)
        .map_err(|_| SystemError::ENOMEM)?;
    participants.extend(devices.values().cloned());
    if let Some(device) = additional {
        participants.push(device.clone());
    }
    Ok(participants)
}

#[unified_init(INITCALL_SUBSYS)]
pub fn root_net_namespace_init() -> Result<(), SystemError> {
    lazy_static::initialize(&NETNS_TEARDOWN_WQ);
    // 创建root网络命名空间的轮询线程
    NetNamespace::create_polling_thread(INIT_NET_NAMESPACE.clone(), "root_netns".to_string());

    // Router/FIB are constructed together with the namespace and remain
    // stable for its entire lifetime. Initialization only attaches the weak
    // namespace reference; replacing the Router here would discard routes
    // imported by devices registered earlier in boot.
    let router = INIT_NET_NAMESPACE.router();
    let mut guard = router.ns.write();
    *guard = INIT_NET_NAMESPACE.self_ref.clone();

    Ok(())
}

/// # 获取下一个网络命名空间计数器的值
fn get_next_netns_counter() -> usize {
    unsafe { NETNS_COUNTER.fetch_add(1, core::sync::atomic::Ordering::SeqCst) }
}

#[derive(Debug)]
pub struct NetNamespace {
    ns_common: NsCommon,
    self_ref: Weak<NetNamespace>,
    _user_ns: Arc<UserNamespace>,
    inner: RwLock<InnerNetNamespace>,
    /// # 轮询线程控制器
    /// 使用弱引用避免 poll 线程持有 netns 强引用，阻止 Drop
    poller: Arc<NetnsPoller>,
    /// # 当前网络命名空间下所有网络接口的列表
    /// 该列表仅应在 **进程上下文** 使用（可睡眠），避免在 hardirq 上下文遍历/加锁。
    /// hardirq 应仅做 `napi_schedule()`（见 `driver/net/irq_handle.rs`）。
    ///
    /// 注意：该结构会在 bind/connect 等路径被访问，且这些路径可能会获取可睡眠的 Mutex，
    /// 因此这里使用可睡眠的 `RwSem`，避免自旋锁 + schedule 的组合导致崩溃。
    device_list: RwSem<NetnsDeviceMap>,
    /// Number of configured explicit IPv4 broadcast addresses. NAPI reads
    /// this without taking topology/address locks under the protocol lock.
    explicit_ipv4_broadcasts: AtomicUsize,
    /// Namespace-relative IDs used when reporting links whose peer lives in
    /// another netns. Weak entries do not prolong a peer namespace's life.
    peer_netns_ids: Mutex<PeerNetnsIds>,
    /// Preallocated payload slot and work item for allocation-free final drop.
    teardown_work: NetnsTeardownWork,
    /// Configured, non-aging neighbors owned by this network namespace.
    neighbor_table: NeighborTable,
    /// Per-netns UDP port reservation and local-delivery table.
    udp_bindings: UdpBindingTable,
    /// Runtime connection/NAT mappings, separate from immutable nft rules.
    conntrack: CtState,
    ipv4_identification: AtomicU16,
    ipv6_fragment_identification: AtomicU32,
    tcp_ports: Arc<crate::net::socket::inet::common::PortManager>,
    tcp_stack: Arc<crate::net::tcp_stack::TcpStack>,
    /// Lock-free read-side snapshot for AF_PACKET delivery from NAPI context.
    packet_sockets: RcuArcSlot<PacketSocketRegistrySnapshot>,
    /// Serializes all plain/fanout topology updates and owns group IDs.
    packet_sockets_writer: Mutex<PacketSocketRegistryWriter>,
    packet_sockets_need_cleanup: AtomicBool,
    ///当前网络命名空间下的桥接设备列表
    bridge_list: RwSem<BTreeMap<String, Arc<BridgeDriver>>>,

    // -- Netlink --
    /// # 当前网络命名空间下的 Netlink 套接字表
    /// 负责绑定netlink套接字的接收队列，以便发送接收消息
    netlink_socket_table: NetlinkSocketTable,
    /// Kept in a Box so final netns release can hand its RCU slot to the
    /// preallocated teardown worker without allocating in a NAPI context.
    nftables: Option<Box<NftNamespaceState>>,
    /// # 当前网络命名空间下的 Netlink 内核套接字
    /// 负责接收并处理 Netlink 消息
    netlink_kernel_socket: RwSem<HashMap<u32, Arc<dyn NetlinkKernelSocket>>>,

    /// AF_UNIX abstract namespace table (scoped to this netns).
    unix_abstract_table: Arc<UnixAbstractTable>,
    /// Per-netns IPv4 ephemeral port range (ip_local_port_range)
    local_port_range: AtomicU32,
    /// Linux /proc/sys/net/ipv4/ip_forward. Default is disabled in each netns.
    ipv4_forwarding: AtomicI32,
    /// Linux /proc/sys/net/ipv6/conf/all/forwarding. Never shares IPv4 state.
    ipv6_forwarding: AtomicI32,
    /// 当前网络命名空间的 loopback 网卡。
    loopback_iface: RcuOptionArcSlot<LoopbackInterface>,
    /// 当前网络命名空间的默认网卡。
    default_iface: RcuOptionArcSlot<DefaultIfaceRef>,
}

#[derive(Debug, Default)]
struct PacketSocketRegistrySnapshot {
    sockets: Vec<Weak<PacketSocket>>,
    groups: Vec<Arc<FanoutGroup>>,
    live_receiver_count: usize,
}

#[derive(Debug)]
struct PacketSocketRegistryWriter {
    /// Authoritative `group id -> group` index used by the write path.
    by_id: HashMap<u16, Arc<FanoutGroup>>,
    /// Group id allocator. Reserves both UNIQUEID-allocated ids and explicit
    /// ids so the two namespaces can never collide.
    id_alloc: IdAllocator,
}

#[derive(Debug, Default)]
struct PeerNetnsIds {
    /// Local ID -> peer. Dead slots are reused only when allocating a new ID.
    slots: Vec<(usize, Weak<NetNamespace>)>,
    /// Global namespace identity -> local ID. GETLINK must not linearly scan
    /// every container peer while holding RTNL.
    by_global: HashMap<usize, i32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PacketSocketCleanupResult {
    Complete,
    Pending,
    AllocationFailed,
}

impl PacketSocketRegistryWriter {
    fn new() -> Self {
        // Group ids occupy the full u16 range; 0 is a valid explicit id.
        Self {
            by_id: HashMap::new(),
            id_alloc: IdAllocator::new(0, u16::MAX as usize + 1)
                .expect("fanout group id allocator"),
        }
    }
}

#[derive(Debug)]
pub struct InnerNetNamespace {
    router: Arc<Router>,
}

struct DefaultIfaceRef {
    iface: Arc<dyn Iface>,
}

impl DefaultIfaceRef {
    fn new(iface: Arc<dyn Iface>) -> Arc<Self> {
        Arc::new(Self { iface })
    }
}

impl core::fmt::Debug for DefaultIfaceRef {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("DefaultIfaceRef")
            .field("nic_id", &self.iface.nic_id())
            .field("iface_name", &self.iface.iface_name())
            .finish()
    }
}

const DEFRAG_PENDING_PACKETS: usize = 128;
const DEFRAG_PENDING_BYTES: usize = 4 * 1024 * 1024;
const DEFRAG_POLL_BATCH: usize = 32;

/// Fragment queues at CT PRE_ROUTING, CT LOCAL_OUT, and stateless LOCAL_IN
/// never merge even if their tuple and identification match.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DefragSource {
    LinkIngress,
    LocalOutput,
    LocalInput,
}

impl DefragSource {
    fn domain(self) -> DefragDomain {
        match self {
            Self::LinkIngress => DefragDomain::pre_routing(0, 0),
            Self::LocalOutput => DefragDomain::local_out(0, 0),
            Self::LocalInput => DefragDomain::local_input(0, 0),
        }
    }

    fn packet_origin(self) -> crate::driver::net::LocalPacketOrigin {
        match self {
            Self::LinkIngress => crate::driver::net::LocalPacketOrigin::LinkIngressPending,
            Self::LocalOutput => crate::driver::net::LocalPacketOrigin::LocalOutput,
            Self::LocalInput => crate::driver::net::LocalPacketOrigin::LocalInputDone,
        }
    }

    fn ipv6_domain(self, ingress_ifindex: u32) -> Ipv6DefragDomain {
        match self {
            Self::LinkIngress => Ipv6DefragDomain::pre_routing(0, ingress_ifindex),
            Self::LocalOutput => Ipv6DefragDomain::local_out(0, ingress_ifindex),
            Self::LocalInput => Ipv6DefragDomain::local_input(0, ingress_ifindex),
        }
    }
}

#[derive(Clone)]
struct FragmentOrigin {
    ingress_ifindex: u32,
    owner_ifindex: u32,
    source_hardware_addr: HardwareAddress,
    source: DefragSource,
    ct_context: Option<Arc<CtPacketContext>>,
    mark: u32,
    broadcast: bool,
}

/// Owned by the receive callback only until the source interface locks have
/// been released. The namespace poller then becomes the sole reassembly owner.
#[derive(Debug)]
pub(crate) struct PendingIpv4Fragment {
    pub(crate) packet: Vec<u8>,
    pub(crate) ingress_ifindex: u32,
    pub(crate) owner_ifindex: u32,
    pub(crate) source_hardware_addr: HardwareAddress,
    pub(crate) source: DefragSource,
    pub(crate) ct_context: Option<Arc<CtPacketContext>>,
    pub(crate) mark: u32,
    pub(crate) broadcast: bool,
}

impl PendingIpv4Fragment {
    fn origin(&self) -> FragmentOrigin {
        FragmentOrigin {
            ingress_ifindex: self.ingress_ifindex,
            owner_ifindex: self.owner_ifindex,
            source_hardware_addr: self.source_hardware_addr,
            source: self.source,
            ct_context: self.ct_context.clone(),
            mark: self.mark,
            broadcast: self.broadcast,
        }
    }
}

#[derive(Debug)]
pub(crate) struct PendingIpv6Fragment {
    pub(crate) packet: Vec<u8>,
    pub(crate) ingress_ifindex: u32,
    pub(crate) owner_ifindex: u32,
    pub(crate) source_hardware_addr: HardwareAddress,
    pub(crate) source: DefragSource,
    pub(crate) ct_context: Option<Arc<CtPacketContext>>,
    pub(crate) mark: u32,
}

impl PendingIpv6Fragment {
    fn origin(&self) -> FragmentOrigin {
        FragmentOrigin {
            ingress_ifindex: self.ingress_ifindex,
            owner_ifindex: self.owner_ifindex,
            source_hardware_addr: self.source_hardware_addr,
            source: self.source,
            ct_context: self.ct_context.clone(),
            mark: self.mark,
            broadcast: false,
        }
    }
}

#[derive(Debug)]
pub(crate) enum PendingIpFragment {
    Ipv4(PendingIpv4Fragment),
    Ipv6(PendingIpv6Fragment),
}

impl PendingIpFragment {
    fn packet_capacity(&self) -> usize {
        match self {
            Self::Ipv4(fragment) => fragment.packet.capacity(),
            Self::Ipv6(fragment) => fragment.packet.capacity(),
        }
    }
}

#[derive(Debug)]
struct DefragPending {
    packets: VecDeque<PendingIpFragment>,
    bytes: usize,
}

impl DefragPending {
    fn prepare() -> Result<Self, SystemError> {
        let mut packets = VecDeque::new();
        packets
            .try_reserve(DEFRAG_PENDING_PACKETS)
            .map_err(|_| SystemError::ENOMEM)?;
        Ok(Self { packets, bytes: 0 })
    }

    /// The ring is reserved before publishing conntrack rules. The rejected
    /// packet remains caller-owned, so even a full-queue drop runs unlocked.
    fn push(&mut self, packet: PendingIpFragment) -> Result<(), PendingIpFragment> {
        let capacity = packet.packet_capacity();
        if self.packets.len() == DEFRAG_PENDING_PACKETS
            || self.bytes.saturating_add(capacity) > DEFRAG_PENDING_BYTES
        {
            return Err(packet);
        }
        debug_assert!(self.packets.capacity() >= DEFRAG_PENDING_PACKETS);
        self.bytes += capacity;
        self.packets.push_back(packet);
        Ok(())
    }

    fn pop(&mut self) -> Option<PendingIpFragment> {
        let packet = self.packets.pop_front()?;
        self.bytes -= packet.packet_capacity();
        Some(packet)
    }
}

#[cfg(test)]
mod defrag_pending_tests {
    use super::*;
    use smoltcp::wire::Ipv4Packet;

    fn packet(bytes: usize) -> PendingIpv4Fragment {
        let mut packet = Vec::new();
        packet.try_reserve_exact(bytes).unwrap();
        PendingIpv4Fragment {
            packet,
            ingress_ifindex: 2,
            owner_ifindex: 2,
            source_hardware_addr: HardwareAddress::Ip,
            source: DefragSource::LinkIngress,
            ct_context: None,
            mark: 0,
            broadcast: false,
        }
    }

    #[test]
    fn preallocated_ring_rejects_full_without_losing_existing_fragments() {
        let mut pending = DefragPending::prepare().unwrap();
        assert!(pending.packets.capacity() >= DEFRAG_PENDING_PACKETS);
        for _ in 0..DEFRAG_PENDING_PACKETS {
            pending.push(PendingIpFragment::Ipv4(packet(0))).unwrap();
        }
        assert!(pending.push(PendingIpFragment::Ipv4(packet(0))).is_err());
        assert_eq!(pending.packets.len(), DEFRAG_PENDING_PACKETS);
        for _ in 0..DEFRAG_PENDING_PACKETS {
            pending.pop().unwrap();
        }
        assert!(pending.pop().is_none());
        assert_eq!(pending.bytes, 0);
    }

    #[test]
    fn pending_byte_limit_charges_allocation_capacity() {
        let mut pending = DefragPending::prepare().unwrap();
        let large = packet(DEFRAG_PENDING_BYTES - 1024);
        let charged = large.packet.capacity();
        pending.push(PendingIpFragment::Ipv4(large)).unwrap();
        assert_eq!(pending.bytes, charged);
        assert!(pending.push(PendingIpFragment::Ipv4(packet(2048))).is_err());
        assert_eq!(pending.bytes, charged);
        pending.pop().unwrap();
        assert_eq!(pending.bytes, 0);
    }

    #[test]
    fn fragment_source_preserves_distinct_domain_and_reinjection_stage() {
        let mut pending = DefragPending::prepare().unwrap();
        let mut output = packet(0);
        output.source = DefragSource::LocalOutput;
        pending.push(PendingIpFragment::Ipv4(packet(0))).unwrap();
        pending.push(PendingIpFragment::Ipv4(output)).unwrap();

        let PendingIpFragment::Ipv4(link) = pending.pop().unwrap() else {
            panic!("expected IPv4 fragment");
        };
        let PendingIpFragment::Ipv4(output) = pending.pop().unwrap() else {
            panic!("expected IPv4 fragment");
        };
        let link = link.origin().source;
        let output = output.origin().source;
        assert_eq!(link, DefragSource::LinkIngress);
        assert_eq!(output, DefragSource::LocalOutput);
        assert_ne!(link.domain(), output.domain());
        assert_eq!(
            link.packet_origin(),
            crate::driver::net::LocalPacketOrigin::LinkIngressPending
        );
        assert_eq!(
            output.packet_origin(),
            crate::driver::net::LocalPacketOrigin::LocalOutput
        );
    }

    #[test]
    fn identical_ipv4_ids_from_link_and_local_output_do_not_mix() {
        fn fragment(first: bool) -> Vec<u8> {
            let mut packet = alloc::vec![0; 28];
            packet[0] = 0x45;
            packet[2..4].copy_from_slice(&28u16.to_be_bytes());
            packet[4..6].copy_from_slice(&7u16.to_be_bytes());
            packet[6..8].copy_from_slice(&(if first { 0x2000u16 } else { 1u16 }).to_be_bytes());
            packet[8] = 64;
            packet[9] = 17;
            packet[12..16].copy_from_slice(&[192, 0, 2, 1]);
            packet[16..20].copy_from_slice(&[192, 0, 2, 2]);
            packet[20..].copy_from_slice(if first { b"abcdefgh" } else { b"ijklmnop" });
            Ipv4Packet::new_unchecked(packet.as_mut_slice()).fill_checksum();
            packet
        }

        let mut assembler = Ipv4Defragmenter::new();
        let link = packet(0).origin();
        let mut output_packet = packet(0);
        output_packet.source = DefragSource::LocalOutput;
        output_packet.ct_context = Some(Arc::new(CtPacketContext::Invalid));
        output_packet.mark = 0x1234;
        output_packet.broadcast = true;
        let output = output_packet.origin();
        let mut output_completion = output.clone();
        output_completion.broadcast = false;
        let first = fragment(true);
        let last = fragment(false);
        let mut link_completion = link.clone();
        link_completion.mark = 0x5678;
        assert!(matches!(
            assembler.submit(&first, link.source.domain(), link.clone(), 0),
            ReassemblyResult::Pending
        ));
        assert!(matches!(
            assembler.submit(&last, output.source.domain(), output_completion, 1),
            ReassemblyResult::Pending
        ));
        let ReassemblyResult::Complete(link_datagram) =
            assembler.submit(&last, link.source.domain(), link_completion, 2)
        else {
            panic!("link fragments must reassemble independently");
        };
        assert_eq!(link_datagram.first_origin.source, DefragSource::LinkIngress);
        assert_eq!(link_datagram.first_origin.mark, 0);
        assert_eq!(link_datagram.completion_origin.mark, 0x5678);
        assert_eq!(
            link_datagram.completion_origin.source,
            DefragSource::LinkIngress
        );
        let ReassemblyResult::Complete(output_datagram) =
            assembler.submit(&first, output.source.domain(), output, 3)
        else {
            panic!("local-output fragments must reassemble independently");
        };
        assert_eq!(
            output_datagram.first_origin.source,
            DefragSource::LocalOutput
        );
        assert_eq!(
            output_datagram.completion_origin.source,
            DefragSource::LocalOutput
        );
        assert!(matches!(
            output_datagram.first_origin.ct_context.as_deref(),
            Some(CtPacketContext::Invalid)
        ));
        assert_eq!(output_datagram.first_origin.mark, 0x1234);
        assert!(output_datagram.first_origin.broadcast);
        assert!(!output_datagram.completion_origin.broadcast);
    }
}

#[derive(Debug)]
struct NetnsPoller {
    netns: Weak<NetNamespace>,
    /// # 用于唤醒网络轮询线程的等待队列
    /// 使用 WaitQueue 的 Waiter/Waker 机制避免唤醒丢失
    wait_queue: WaitQueue,
    /// # 标记是否有待处理的网络事件
    /// 用于避免唤醒丢失：当 poll 线程正在 poll 时收到的唤醒请求会设置此标志，
    /// poll 线程在进入等待前会检查此标志
    poll_pending: AtomicBool,
    /// Topology cleanup wakes the poller without being treated as network I/O.
    cleanup_pending: AtomicBool,
    /// Monotonic notification sequence for future protocol deadline changes.
    /// Unlike `poll_pending`, this only requests a timeout rescan.
    deadline_generation: AtomicU64,
    /// NAPI only moves owned fragments into this preallocated ring. Reassembly
    /// and expiry run in the existing namespace polling thread, never here.
    defrag_pending: SpinLock<Option<DefragPending>>,
    defrag_enabled: AtomicBool,
    ipv6_defrag_enabled: AtomicBool,
    /// # 轮询线程的 PCB（用于 stop）
    thread: RwSem<Option<Arc<ProcessControlBlock>>>,
}

impl NetnsPoller {
    fn new(netns: Weak<NetNamespace>) -> Arc<Self> {
        Arc::new(Self {
            netns,
            wait_queue: WaitQueue::default(),
            poll_pending: AtomicBool::new(false),
            cleanup_pending: AtomicBool::new(false),
            deadline_generation: AtomicU64::new(0),
            defrag_pending: SpinLock::new(None),
            defrag_enabled: AtomicBool::new(false),
            ipv6_defrag_enabled: AtomicBool::new(false),
            thread: RwSem::new(None),
        })
    }

    fn prepare_ip_defrag(&self) -> Result<(), SystemError> {
        if self.defrag_pending.lock().is_some() {
            return Ok(());
        }
        // Fallible allocation is exclusively in the nft transaction's process
        // context, before any rule can hand fragments to this ring.
        let prepared = DefragPending::prepare()?;
        let mut pending = self.defrag_pending.lock();
        if pending.is_none() {
            *pending = Some(prepared);
        }
        Ok(())
    }

    fn enable_ipv4_defrag(&self) -> Result<(), SystemError> {
        if self.defrag_pending.lock().is_none() {
            return Err(SystemError::EINVAL);
        }
        self.defrag_enabled.store(true, Ordering::Release);
        Ok(())
    }

    fn enable_ipv6_defrag(&self) -> Result<(), SystemError> {
        if self.defrag_pending.lock().is_none() {
            return Err(SystemError::EINVAL);
        }
        self.ipv6_defrag_enabled.store(true, Ordering::Release);
        Ok(())
    }

    fn queue_ip_fragment(&self, packet: PendingIpFragment) -> Result<(), PendingIpFragment> {
        let enabled = match &packet {
            PendingIpFragment::Ipv4(_) => self.defrag_enabled.load(Ordering::Acquire),
            PendingIpFragment::Ipv6(_) => self.ipv6_defrag_enabled.load(Ordering::Acquire),
        };
        if !enabled {
            return Err(packet);
        }
        let queued = {
            let mut pending = self.defrag_pending.lock();
            match pending.as_mut() {
                Some(pending) => pending.push(packet),
                None => Err(packet),
            }
        };
        if queued.is_ok() {
            self.notify_deadline_changed();
        }
        queued
    }

    fn pop_ip_fragment(&self) -> Option<PendingIpFragment> {
        self.defrag_pending.lock().as_mut()?.pop()
    }

    fn has_pending_ip_fragments(&self) -> bool {
        self.defrag_pending
            .lock()
            .as_ref()
            .is_some_and(|queue| !queue.packets.is_empty())
    }

    fn reinject_ip(
        netns: &NetNamespace,
        packet: Vec<u8>,
        first_origin: FragmentOrigin,
        completion: FragmentOrigin,
        broadcast: bool,
    ) {
        let source_mac = match first_origin.source_hardware_addr {
            HardwareAddress::Ethernet(mac) => mac,
            HardwareAddress::Ip
                if completion.owner_ifindex == crate::net::LOOPBACK_IFINDEX as u32 =>
            {
                // A medium-IP local receive token has no Ethernet header.
                EthernetAddress([0; 6])
            }
            _ => return,
        };
        // Interface identity is a trusted receive-token value, not inferred
        // from the IP header. The completion fragment supplies the receive
        // device; the offset-zero fragment supplies the source MAC.
        let owner = {
            let devices = netns.device_list.read();
            devices.get(&(completion.owner_ifindex as usize)).cloned()
        };
        let Some(owner) = owner else {
            return;
        };
        let owner_epoch = owner.common().namespace_epoch();
        if owner_epoch & 1 != 0
            || !owner
                .net_namespace()
                .is_some_and(|current| core::ptr::eq(current.as_ref(), netns))
        {
            return;
        }
        let ct_context = match completion.source {
            DefragSource::LocalOutput => Some(
                first_origin
                    .ct_context
                    .as_deref()
                    .cloned()
                    .unwrap_or(CtPacketContext::Untracked),
            ),
            // LOCAL_IN already accepted every fragment before assembly.
            // A later ruleset must not classify the completed old packet as
            // a new flow or discard an otherwise valid local datagram.
            DefragSource::LocalInput => Some(CtPacketContext::Untracked),
            DefragSource::LinkIngress => None,
        };
        if let Err(error) = crate::driver::net::inject_owned_local_ip_packet_if_epoch(
            owner.as_ref(),
            completion.ingress_ifindex,
            source_mac,
            packet,
            broadcast,
            completion.source.packet_origin(),
            ct_context,
            first_origin.mark,
            Some(owner_epoch),
        ) {
            log::debug!("reassembled IP ingress discarded: {:?}", error);
        }
    }

    fn reinject_ipv4(netns: &NetNamespace, datagram: ReassembledIpv4<FragmentOrigin>) {
        let ReassembledIpv4 {
            packet,
            first_origin,
            completion_origin,
        } = datagram;
        // LOCAL_OUT has already selected its broadcast route. Keep that
        // offset-zero decision through reassembly instead of trying to infer
        // an explicit IFA_BROADCAST from the completed packet's address bits.
        let broadcast = first_origin.broadcast || packet[16..20] == [255; 4];
        Self::reinject_ip(netns, packet, first_origin, completion_origin, broadcast);
    }

    fn reinject_ipv6(netns: &NetNamespace, datagram: ReassembledIpv6<FragmentOrigin>) {
        let ReassembledIpv6 {
            packet,
            first_origin,
            completion_origin,
        } = datagram;
        Self::reinject_ip(netns, packet, first_origin, completion_origin, false);
    }

    fn start(self: &Arc<Self>, name: String) {
        let poller = self.clone();
        let closure: Box<dyn Fn() -> i32 + Send + Sync> = Box::new(move || {
            poller.polling();
            0
        });
        let pcb = KernelThreadMechanism::create_and_run(
            KernelThreadClosure::EmptyClosure((closure, ())),
            name,
        )
        .expect("create net_poll thread for net namespace failed");
        // 避免轮询线程通过 nsproxy 持有 netns 强引用导致无法释放
        pcb.set_nsproxy(NsProxy::new_root());
        *self.thread.write() = Some(pcb);
    }

    fn stop(&self) {
        let pcb = self.thread.write().take();
        if let Some(pcb) = pcb {
            // 唤醒等待中的 poll 线程，确保其能看到 should_stop 标志。
            //
            // 重要：stop 可能由 poller 线程自身触发（例如 poller 线程释放最后一个 netns Arc，
            // 进入 NetNamespace::drop）。此时也必须设置 pending 并唤醒/自唤醒，避免在 timeout=None
            // 的 wait_event 上永久睡眠。
            self.poll_pending.store(true, Ordering::Release);
            self.wait_queue.wake_all();
            let _ = KernelThreadMechanism::request_stop(&pcb);
        }
    }

    fn notify_network(&self) -> (bool, usize) {
        let was_pending = self.poll_pending.swap(true, Ordering::AcqRel);
        let woken = self.wait_queue.wake_all();
        (!was_pending, woken)
    }

    /// Wake only the topology-cleanup worker. This is safe from the NAPI read
    /// path and deliberately does not turn cleanup into an interface poll.
    fn notify_cleanup(&self) {
        self.cleanup_pending.store(true, Ordering::Release);
        self.wait_queue.wake_all();
    }

    /// Notify the poller that its previously computed timeout may be stale.
    ///
    /// This path is NAPI-safe: it only touches atomics and the wait queue.
    fn notify_deadline_changed(&self) {
        self.deadline_generation.fetch_add(1, Ordering::AcqRel);
        self.wait_queue.wake_all();
    }

    /// Run one bounded batch for an interface without NAPI.
    ///
    /// Returns `true` when the interface still reports immediate work. The
    /// caller must publish another network wake instead of monopolizing this
    /// netns worker until quiescence.
    fn poll_direct_batch(iface: &Arc<dyn Iface>) -> bool {
        const DIRECT_POLL_BATCH: usize = 64;

        for _ in 0..DIRECT_POLL_BATCH {
            match iface.common().poll_scope() {
                IfacePollScope::None => return false,
                IfacePollScope::LocalOnly | IfacePollScope::Full if !iface.poll() => return false,
                IfacePollScope::LocalOnly | IfacePollScope::Full => {}
            }
        }
        true
    }

    fn polling(&self) {
        let mut cleanup_retry_delay = PACKET_SOCKET_CLEANUP_RETRY_MIN;
        let mut cleanup_retry_at = None;
        // The namespace poller owns reassembly exclusively. Neither NAPI nor
        // the pending-ring spinlock performs fragment allocation or expiry.
        let mut ipv4_defrag = Ipv4Defragmenter::new();
        let mut ipv6_defrag = Ipv6Defragmenter::new();
        loop {
            if KernelThreadMechanism::should_stop(&ProcessManager::current_pcb()) {
                break;
            }

            let netns = match self.netns.upgrade() {
                Some(netns) => netns,
                None => {
                    log::info!("netns poller exit: netns dropped");
                    break;
                }
            };

            let nsid = netns.ns_common.nsid.data();
            let cleanup_now_us = Instant::now().total_micros() as u64;
            if cleanup_retry_at.is_none_or(|deadline| cleanup_now_us >= deadline) {
                match netns.cleanup_packet_sockets() {
                    PacketSocketCleanupResult::Complete => {
                        cleanup_retry_delay = PACKET_SOCKET_CLEANUP_RETRY_MIN;
                        cleanup_retry_at = None;
                    }
                    PacketSocketCleanupResult::Pending => {
                        cleanup_retry_delay = PACKET_SOCKET_CLEANUP_RETRY_MIN;
                        cleanup_retry_at = None;
                    }
                    PacketSocketCleanupResult::AllocationFailed => {
                        cleanup_retry_at =
                            Some(cleanup_now_us.saturating_add(cleanup_retry_delay.total_micros()));
                        cleanup_retry_delay = Duration::from_micros(core::cmp::min(
                            cleanup_retry_delay.total_micros().saturating_mul(2),
                            PACKET_SOCKET_CLEANUP_RETRY_MAX.total_micros(),
                        ));
                    }
                }
            }

            // Cleanup may wait on packet-socket ownership. Deadline
            // classification and timeout calculation must use a fresh clock
            // sample so time spent there cannot postpone an already-due TCP
            // timer by one additional timeout interval.
            let mut observed_generation = self.deadline_generation.load(Ordering::Acquire);
            let deadline_now = Instant::now();
            let mut deadline_now_us = deadline_now.total_micros() as u64;
            netns.conntrack.expire_due(deadline_now);
            if ipv4_defrag
                .next_deadline_us()
                .is_some_and(|deadline| deadline <= deadline_now_us)
            {
                ipv4_defrag.expire(deadline_now_us);
            }
            if ipv6_defrag
                .next_deadline_us()
                .is_some_and(|deadline| deadline <= deadline_now_us)
            {
                ipv6_defrag.expire(deadline_now_us);
            }
            for _ in 0..DEFRAG_POLL_BATCH {
                let Some(fragment) = self.pop_ip_fragment() else {
                    break;
                };
                let now_us = Instant::now().total_micros().max(0) as u64;
                match fragment {
                    PendingIpFragment::Ipv4(fragment) => {
                        let origin = fragment.origin();
                        let domain = fragment.source.domain();
                        if let ReassemblyResult::Complete(datagram) =
                            ipv4_defrag.submit(&fragment.packet, domain, origin, now_us)
                        {
                            Self::reinject_ipv4(&netns, datagram);
                        }
                    }
                    PendingIpFragment::Ipv6(fragment) => {
                        let origin = fragment.origin();
                        let domain = fragment.source.ipv6_domain(fragment.ingress_ifindex);
                        if let Ipv6ReassemblyResult::Complete(datagram) =
                            ipv6_defrag.submit(&fragment.packet, domain, origin, now_us)
                        {
                            Self::reinject_ipv6(&netns, datagram);
                        }
                    }
                }
            }

            // TCP protocol ownership is namespace-wide, independent of netdev
            // UP/NAPI state. One batch per scheduler iteration prevents every
            // interface from scanning the same TCP socket collection.
            if netns.tcp_stack.poll_due(deadline_now_us) {
                netns.tcp_stack.poll();
                // Still service physical work during sustained TCP input.
                // Resnapshot after the batch before calculating our timeout.
                observed_generation = self.deadline_generation.load(Ordering::Acquire);
                deadline_now_us = Instant::now().total_micros() as u64;
            }

            // Classify and atomically claim due protocol deadlines. The
            // device-list lock is only used for topology lookup; no direct
            // protocol poll or yield is performed while it is held.
            let mut next_us = match (cleanup_retry_at, netns.tcp_stack.deadline_us()) {
                (Some(cleanup), Some(tcp)) => Some(core::cmp::min(cleanup, tcp)),
                (cleanup, tcp) => cleanup.or(tcp),
            };
            if let Some(ct_deadline) = netns.conntrack.next_expiry() {
                let ct_us = ct_deadline.total_micros().max(0) as u64;
                next_us = Some(next_us.map_or(ct_us, |current| current.min(ct_us)));
            }
            if let Some(defrag_deadline) = ipv4_defrag.next_deadline_us() {
                next_us =
                    Some(next_us.map_or(defrag_deadline, |current| current.min(defrag_deadline)));
            }
            if let Some(defrag_deadline) = ipv6_defrag.next_deadline_us() {
                next_us =
                    Some(next_us.map_or(defrag_deadline, |current| current.min(defrag_deadline)));
            }
            if self.has_pending_ip_fragments() {
                next_us = Some(deadline_now_us);
            }
            let mut direct_due = Vec::new();
            {
                let devices = netns.device_list.read();
                for (_, iface) in devices.iter() {
                    if iface.common().poll_scope() == IfacePollScope::None {
                        continue;
                    }

                    let napi = iface.napi_struct();
                    if napi.as_deref().is_some_and(napi_is_disabled) {
                        continue;
                    }

                    match iface.common().classify_poll_deadline(deadline_now_us) {
                        DueResult::Disarmed => {}
                        DueResult::Future(us) => {
                            next_us = Some(match next_us {
                                Some(cur) => core::cmp::min(cur, us),
                                None => us,
                            });
                        }
                        DueResult::Claimed { claims, next } => {
                            if let Some(us) = next {
                                next_us = Some(match next_us {
                                    Some(cur) => core::cmp::min(cur, us),
                                    None => us,
                                });
                            }
                            match napi {
                                Some(napi) => match napi_schedule(napi) {
                                    NapiScheduleResult::Accepted => {}
                                    NapiScheduleResult::Disabled | NapiScheduleResult::Detached => {
                                        iface.common().restore_poll_deadline(claims);
                                    }
                                },
                                None => direct_due.push((iface.clone(), claims)),
                            }
                        }
                    }
                }
            }

            if !direct_due.is_empty() {
                drop(netns);
                for (iface, claims) in direct_due {
                    if iface.common().poll_scope() == IfacePollScope::None {
                        iface.common().restore_poll_deadline(claims);
                        continue;
                    }
                    if Self::poll_direct_batch(&iface) {
                        self.notify_network();
                    }
                }
                // A direct poll may have published a new future deadline.
                // Rescan from a fresh generation snapshot before sleeping.
                continue;
            }

            // Scheduling due interfaces can contend on device-side locks and
            // a namespace may contain many interfaces. Re-sample immediately
            // before sleeping so scan time is not added to the next deadline.
            let sleep_now_us = Instant::now().total_micros() as u64;
            let timeout = next_us.map(|us| {
                let delta = us.saturating_sub(sleep_now_us);
                Duration::from_micros(core::cmp::max(1, delta))
            });
            log::trace!(
                "netns scheduler sleep: nsid={} timeout_us={:?}",
                nsid,
                timeout.map(|d| d.total_micros())
            );

            // 释放 netns 引用再进入等待，避免 poll 线程长期持有 netns 阻止 Drop。
            drop(netns);

            // 等待事件唤醒（IRQ/lo Tx 等）或 timeout（smoltcp timer deadline）。
            // Keep cleanup and network wake reasons separate: only the latter
            // should schedule interface NAPI below.
            match self.wait_queue.wait_event_uninterruptible_timeout(
                || {
                    self.poll_pending.load(Ordering::Acquire)
                        || self.cleanup_pending.load(Ordering::Acquire)
                        || self.deadline_generation.load(Ordering::Acquire) != observed_generation
                },
                timeout,
            ) {
                Ok(()) | Err(SystemError::EAGAIN_OR_EWOULDBLOCK) => {}
                Err(e) => {
                    log::warn!("netns scheduler sleep error: {:?}", e);
                }
            }

            let network_pending = self.poll_pending.swap(false, Ordering::AcqRel);
            self.cleanup_pending.swap(false, Ordering::AcqRel);
            if KernelThreadMechanism::should_stop(&ProcessManager::current_pcb()) {
                break;
            }
            if !network_pending {
                continue;
            }

            let netns = match self.netns.upgrade() {
                Some(netns) => netns,
                None => break,
            };
            let mut direct_poll = Vec::new();
            {
                let devices = netns.device_list.read();
                // Event-driven work is scheduled once; NAPI performs bounded
                // polling and records concurrent requests through MISSED.
                for (_, iface) in devices.iter() {
                    if iface.common().poll_scope() == IfacePollScope::None {
                        continue;
                    }
                    if let Some(napi) = iface.napi_struct() {
                        napi_schedule(napi);
                    } else {
                        direct_poll.push(iface.clone());
                    }
                }
            }
            drop(netns);
            for iface in direct_poll {
                if Self::poll_direct_batch(&iface) {
                    self.notify_network();
                }
            }
        }
    }
}

impl InnerNetNamespace {
    pub fn router(&self) -> &Arc<Router> {
        &self.router
    }
}

impl NetNamespace {
    pub(crate) fn has_explicit_ipv4_broadcast(&self) -> bool {
        self.explicit_ipv4_broadcasts.load(Ordering::Acquire) != 0
    }

    /// RTNL serializes address and device publishers. Publish this before
    /// releasing the FIB transaction so a poller cannot use an old backend.
    pub(crate) fn publish_ipv4_broadcast_count_change(&self, before: usize, after: usize) {
        if after > before {
            self.explicit_ipv4_broadcasts
                .fetch_add(after - before, Ordering::Release);
        } else if before > after {
            let old = self
                .explicit_ipv4_broadcasts
                .fetch_sub(before - after, Ordering::Release);
            debug_assert!(old >= before - after);
        }
    }

    pub fn new_root() -> Arc<Self> {
        let inner = InnerNetNamespace {
            router: Router::new("root_netns_router".to_string()),
        };

        let ns_common = NsCommon::new(0, NamespaceType::Net);
        let unix_abstract_table = UnixAbstractTable::new(ns_common.nsid.data());

        let netns = Arc::new_cyclic(|self_ref| Self {
            ns_common: ns_common.clone(),
            self_ref: self_ref.clone(),
            _user_ns: crate::process::namespace::user_namespace::INIT_USER_NAMESPACE.clone(),
            inner: RwLock::new(inner),
            poller: NetnsPoller::new(self_ref.clone()),
            device_list: RwSem::new(BTreeMap::new()),
            explicit_ipv4_broadcasts: AtomicUsize::new(0),
            peer_netns_ids: Mutex::new(PeerNetnsIds::default()),
            teardown_work: NetnsTeardownWork::new(),
            neighbor_table: NeighborTable::new(),
            udp_bindings: UdpBindingTable::default(),
            conntrack: CtState::new(),
            ipv4_identification: AtomicU16::new(crate::arch::rand::rand() as u16),
            ipv6_fragment_identification: AtomicU32::new(crate::arch::rand::rand() as u32),
            tcp_ports: Arc::new(crate::net::socket::inet::common::PortManager::default()),
            tcp_stack: Arc::new(crate::net::tcp_stack::TcpStack::new(self_ref.clone())),
            packet_sockets: RcuArcSlot::new(Arc::new(PacketSocketRegistrySnapshot::default())),
            packet_sockets_writer: Mutex::new(PacketSocketRegistryWriter::new()),
            packet_sockets_need_cleanup: AtomicBool::new(false),
            bridge_list: RwSem::new(BTreeMap::new()),
            netlink_socket_table: NetlinkSocketTable::default(),
            nftables: Some(Box::new(NftNamespaceState::new())),
            netlink_kernel_socket: RwSem::new(generate_supported_netlink_kernel_sockets()),
            unix_abstract_table: unix_abstract_table.clone(),
            local_port_range: AtomicU32::new(
                crate::net::socket::inet::common::port::DEFAULT_LOCAL_PORT_RANGE,
            ),
            ipv4_forwarding: AtomicI32::new(0),
            ipv6_forwarding: AtomicI32::new(0),
            loopback_iface: RcuOptionArcSlot::new_none(),
            default_iface: RcuOptionArcSlot::new_none(),
        });

        log::info!("Initialized root net namespace");
        netns
    }

    pub fn new_empty(user_ns: Arc<UserNamespace>) -> Result<Arc<Self>, SystemError> {
        let counter = get_next_netns_counter();
        let loopback = crate::driver::net::loopback::LoopbackInterface::new_with_ifindex(
            crate::driver::net::loopback::LoopbackDriver::default(),
            crate::net::LOOPBACK_IFINDEX,
        );
        let inner = InnerNetNamespace {
            router: Router::new(format!("netns_router_{}", counter)),
        };

        let ns_common = NsCommon::new(0, NamespaceType::Net);
        let unix_abstract_table = UnixAbstractTable::new(ns_common.nsid.data());

        let netns = Arc::new_cyclic(|self_ref| Self {
            ns_common: ns_common.clone(),
            self_ref: self_ref.clone(),
            _user_ns: user_ns,
            inner: RwLock::new(inner),
            poller: NetnsPoller::new(self_ref.clone()),
            device_list: RwSem::new(BTreeMap::new()),
            explicit_ipv4_broadcasts: AtomicUsize::new(0),
            peer_netns_ids: Mutex::new(PeerNetnsIds::default()),
            teardown_work: NetnsTeardownWork::new(),
            neighbor_table: NeighborTable::new(),
            udp_bindings: UdpBindingTable::default(),
            conntrack: CtState::new(),
            ipv4_identification: AtomicU16::new(crate::arch::rand::rand() as u16),
            ipv6_fragment_identification: AtomicU32::new(crate::arch::rand::rand() as u32),
            tcp_ports: Arc::new(crate::net::socket::inet::common::PortManager::default()),
            tcp_stack: Arc::new(crate::net::tcp_stack::TcpStack::new(self_ref.clone())),
            packet_sockets: RcuArcSlot::new(Arc::new(PacketSocketRegistrySnapshot::default())),
            packet_sockets_writer: Mutex::new(PacketSocketRegistryWriter::new()),
            packet_sockets_need_cleanup: AtomicBool::new(false),
            bridge_list: RwSem::new(BTreeMap::new()),
            netlink_socket_table: NetlinkSocketTable::default(),
            nftables: Some(Box::new(NftNamespaceState::new())),
            netlink_kernel_socket: RwSem::new(generate_supported_netlink_kernel_sockets()),
            unix_abstract_table: unix_abstract_table.clone(),
            local_port_range: AtomicU32::new(
                crate::net::socket::inet::common::port::DEFAULT_LOCAL_PORT_RANGE,
            ),
            ipv4_forwarding: AtomicI32::new(0),
            ipv6_forwarding: AtomicI32::new(0),
            loopback_iface: RcuOptionArcSlot::new_some(loopback.clone()),
            default_iface: RcuOptionArcSlot::new_none(),
        });

        *netns.router().ns.write() = netns.self_ref.clone();

        // Linux 语义：每个 netns 都需要一个可被唤醒的轮询线程来推进协议栈。
        // 否则像 lo 这样的设备在 Tx 后仅通过 wakeup_poll_thread() 触发下一次 poll，
        // 若此处不记录 pcb，后续将无法唤醒，从而导致 TCP connect/accept 等卡死。
        Self::create_polling_thread(netns.clone(), format!("netns_{}", counter));
        // Per-netns loopback has no global sysfs projection. The shared
        // registration lifecycle makes it PRESENT; as in Linux's
        // loopback_net_init(), it remains administratively down until
        // userspace opens the link.
        crate::driver::net::register_netdevice(&netns, loopback)?;

        Ok(netns)
    }

    pub fn user_ns(&self) -> &Arc<UserNamespace> {
        &self._user_ns
    }

    /// Allocate the ID of `peer` as seen from this namespace. The ID is
    /// stable while the peer is alive and may be reused after its last owner
    /// goes away, matching the lifetime of Linux's per-netns peer IDs.
    pub(crate) fn peer_netnsid(&self, peer: &Arc<NetNamespace>) -> Result<i32, SystemError> {
        let mut ids = self.peer_netns_ids.lock();
        let global = peer.ns_common().nsid.data();
        if let Some(id) = ids.by_global.get(&global) {
            return Ok(*id);
        }
        let reusable = ids
            .slots
            .iter()
            .position(|(_, candidate)| candidate.upgrade().is_none());
        ids.by_global
            .try_reserve(1)
            .map_err(|_| SystemError::ENOMEM)?;
        let id = if let Some(id) = reusable {
            let old_global = ids.slots[id].0;
            ids.by_global.remove(&old_global);
            ids.slots[id] = (global, Arc::downgrade(peer));
            id
        } else {
            let id = ids.slots.len();
            i32::try_from(id).map_err(|_| SystemError::ENOSPC)?;
            ids.slots.try_reserve(1).map_err(|_| SystemError::ENOMEM)?;
            ids.slots.push((global, Arc::downgrade(peer)));
            id
        };
        let id = i32::try_from(id).map_err(|_| SystemError::ENOSPC)?;
        ids.by_global.insert(global, id);
        Ok(id)
    }

    pub(super) fn copy_net_ns(
        &self,
        clone_flags: &CloneFlags,
        user_ns: Arc<UserNamespace>,
    ) -> Result<Arc<Self>, SystemError> {
        if !clone_flags.contains(CloneFlags::CLONE_NEWNET) {
            return Ok(self.self_ref.upgrade().unwrap());
        }

        Self::new_empty(user_ns)
    }

    pub fn device_list_mut(&self) -> RwSemWriteGuard<'_, BTreeMap<usize, Arc<dyn Iface>>> {
        self.device_list.write()
    }

    pub fn device_list(&self) -> RwSemReadGuard<'_, BTreeMap<usize, Arc<dyn Iface>>> {
        self.device_list.read()
    }

    pub(crate) fn neighbor_table(&self) -> &NeighborTable {
        &self.neighbor_table
    }

    pub(crate) fn udp_bindings(&self) -> &UdpBindingTable {
        &self.udp_bindings
    }

    /// Allocate an IPv4 datagram ID before local fragmentation. A shared
    /// per-netns sequence avoids collisions between different UDP sockets
    /// that transmit fragments to the same destination concurrently.
    pub(crate) fn next_ipv4_identification(&self) -> u16 {
        self.ipv4_identification.fetch_add(1, Ordering::Relaxed)
    }

    /// Fragment IDs are shared by all local IPv6 sockets in this namespace.
    pub(crate) fn next_ipv6_fragment_identification(&self) -> u32 {
        self.ipv6_fragment_identification
            .fetch_add(1, Ordering::Relaxed)
    }

    pub(crate) fn tcp_ports(&self) -> &Arc<crate::net::socket::inet::common::PortManager> {
        &self.tcp_ports
    }

    pub fn tcp_stack(&self) -> &Arc<crate::net::tcp_stack::TcpStack> {
        &self.tcp_stack
    }

    pub fn wakeup_tcp_poll(&self) {
        self.tcp_stack.request_poll();
    }

    pub fn register_packet_socket(&self, socket: Weak<PacketSocket>) -> Result<(), SystemError> {
        let writer = self.packet_sockets_writer.lock();
        let current = self.packet_sockets.load();
        let mut sockets = Vec::new();
        sockets
            .try_reserve_exact(current.sockets.len().saturating_add(1))
            .map_err(|_| SystemError::ENOMEM)?;
        sockets.extend(current.sockets.iter().cloned());
        sockets.retain(|entry| entry.upgrade().is_some());
        if !sockets.iter().any(|entry| Weak::ptr_eq(entry, &socket)) {
            sockets.push(socket);
        }
        let snapshot = self.prepare_packet_topology_update(&writer, sockets, None)?;
        self.commit_packet_topology(snapshot);
        Ok(())
    }

    pub fn unregister_packet_socket(&self, socket: &Weak<PacketSocket>) {
        if let Some(socket) = socket.upgrade() {
            socket.deactivate_packet_registry();
        }
        if self.try_unregister_packet_socket(socket).is_err() {
            // close(2) cannot be retried after the fd is detached. Keep the
            // old RCU snapshot valid, mark this member orphaned, and let the
            // fallible poller cleanup retry without failing the close path.
            if let Some(socket) = socket.upgrade() {
                socket.clear_fanout_membership();
            }
            self.request_packet_socket_cleanup();
        }
    }

    /// Coalesce stale-topology notifications and wake only the poller. Unlike
    /// `wakeup_poll_thread`, this path never reads `device_list` from NAPI.
    fn request_packet_socket_cleanup(&self) {
        if !self
            .packet_sockets_need_cleanup
            .swap(true, Ordering::AcqRel)
        {
            self.poller.notify_cleanup();
        }
    }

    fn try_unregister_packet_socket(&self, socket: &Weak<PacketSocket>) -> Result<(), SystemError> {
        let socket_arc = socket.upgrade();
        let group_id = socket_arc
            .as_ref()
            .and_then(|socket| socket.fanout_group_id());
        let mut writer = self.packet_sockets_writer.lock();
        let current = self.packet_sockets.load();
        let mut sockets = Vec::new();
        sockets
            .try_reserve_exact(current.sockets.len())
            .map_err(|_| SystemError::ENOMEM)?;
        sockets.extend(current.sockets.iter().cloned());
        sockets.retain(|entry| entry.upgrade().is_some() && !Weak::ptr_eq(entry, socket));

        let update = match group_id.and_then(|id| writer.by_id.get(&id).map(|group| (id, group))) {
            Some((id, group)) => Some((id, group.try_without_member(socket)?)),
            None => None,
        };
        let snapshot = self.prepare_packet_topology_update(&writer, sockets, update.as_ref())?;
        self.commit_packet_topology(snapshot);
        if let Some((id, replacement)) = update {
            if replacement.member_count() == 0 {
                writer.by_id.remove(&id);
                writer.id_alloc.free(id as usize);
            } else {
                writer.by_id.insert(id, replacement);
            }
        }
        if let Some(socket) = socket_arc {
            socket.clear_fanout_membership();
        }
        Ok(())
    }

    fn cleanup_packet_sockets(&self) -> PacketSocketCleanupResult {
        if !self
            .packet_sockets_need_cleanup
            .swap(false, Ordering::AcqRel)
        {
            return PacketSocketCleanupResult::Complete;
        }
        let mut writer = self.packet_sockets_writer.lock();
        let current = self.packet_sockets.load();
        let mut sockets = Vec::new();
        if sockets.try_reserve_exact(current.sockets.len()).is_err() {
            self.packet_sockets_need_cleanup
                .store(true, Ordering::Release);
            return PacketSocketCleanupResult::AllocationFailed;
        }
        sockets.extend(current.sockets.iter().cloned());
        sockets.retain(|entry| {
            entry
                .upgrade()
                .is_some_and(|socket| socket.is_packet_registry_active())
        });

        let mut groups = Vec::new();
        let mut updates = Vec::new();
        if groups.try_reserve_exact(writer.by_id.len()).is_err()
            || updates.try_reserve_exact(writer.by_id.len()).is_err()
        {
            self.packet_sockets_need_cleanup
                .store(true, Ordering::Release);
            return PacketSocketCleanupResult::AllocationFailed;
        }
        for (id, group) in writer.by_id.iter() {
            match group.try_without_dead_members() {
                Ok(Some(cleaned)) => {
                    if cleaned.member_count() != 0 {
                        groups.push(cleaned.clone());
                    }
                    updates.push((*id, cleaned));
                }
                Ok(None) => groups.push(group.clone()),
                Err(_) => {
                    self.packet_sockets_need_cleanup
                        .store(true, Ordering::Release);
                    return PacketSocketCleanupResult::AllocationFailed;
                }
            }
        }
        let Ok(snapshot) = Self::try_packet_topology(sockets, groups) else {
            self.packet_sockets_need_cleanup
                .store(true, Ordering::Release);
            return PacketSocketCleanupResult::AllocationFailed;
        };
        self.commit_packet_topology(snapshot);
        for (id, replacement) in updates {
            if replacement.member_count() == 0 {
                writer.by_id.remove(&id);
                writer.id_alloc.free(id as usize);
            } else {
                writer.by_id.insert(id, replacement);
            }
        }
        if self.packet_sockets_need_cleanup.load(Ordering::Acquire) {
            PacketSocketCleanupResult::Pending
        } else {
            PacketSocketCleanupResult::Complete
        }
    }

    /// Deliver an ingress frame without taking a sleeping lock or allocating a
    /// temporary registry copy in the NAPI read-side path.
    ///
    /// The single snapshot contains both plain sockets and immutable fanout
    /// groups, so a join/close transition cannot expose a socket in both (or
    /// neither) topology to one reader.
    pub(crate) fn deliver_to_packet_sockets(&self, ingress: PacketIngressMetadata, frame: &[u8]) {
        let snapshot = self.packet_sockets.load();
        let mut stale = false;
        for socket in snapshot.sockets.iter() {
            match socket.upgrade() {
                Some(socket) if socket.is_packet_registry_active() => {
                    socket.deliver(ingress, frame);
                }
                Some(_) | None => stale = true,
            }
        }
        let mut protocol_cache = None;
        let mut flow_hash_cache = None;
        for group in snapshot.groups.iter() {
            if group.deliver(ingress, frame, &mut protocol_cache, &mut flow_hash_cache) {
                stale = true;
            }
        }
        if stale {
            self.request_packet_socket_cleanup();
        }
    }

    pub fn has_packet_sockets(&self) -> bool {
        self.packet_sockets.load().live_receiver_count != 0
    }

    /// Join (creating if necessary) a fanout group.
    ///
    /// Move `socket` from the plain list into a group with one RCU publication.
    /// The caller holds the socket bind lock, fixing the global lock order at
    /// `bind_lock -> packet_sockets_writer`.
    pub(crate) fn fanout_group_join(
        &self,
        socket: &Arc<PacketSocket>,
        params: FanoutJoinParams,
    ) -> Result<(), SystemError> {
        let mut writer = self.packet_sockets_writer.lock();
        if socket.has_fanout_group() {
            return Err(SystemError::EALREADY);
        }
        let socket_ref = socket.self_ref();
        let current = self.packet_sockets.load();
        if !current
            .sockets
            .iter()
            .any(|entry| Weak::ptr_eq(entry, &socket_ref))
        {
            return Err(SystemError::EINVAL);
        }

        let mut reserved_new_id = None;
        let group: Arc<FanoutGroup> = if params.unique {
            writer
                .by_id
                .try_reserve(1)
                .map_err(|_| SystemError::ENOMEM)?;
            let new_id = writer.id_alloc.alloc().ok_or(SystemError::ENOMEM)? as u16;
            reserved_new_id = Some(new_id);
            let group = match FanoutGroup::try_new(new_id, params, socket_ref.clone()) {
                Ok(group) => group,
                Err(err) => {
                    writer.id_alloc.free(new_id as usize);
                    return Err(err);
                }
            };
            group
        } else {
            match writer.by_id.get(&params.id_req).cloned() {
                Some(mut existing) => {
                    if let Some(compacted) = existing.try_without_dead_members()? {
                        existing = compacted;
                    }
                    if existing.member_count() == 0 {
                        FanoutGroup::try_new(params.id_req, params, socket_ref.clone())?
                    } else {
                        if !existing.matches(params) {
                            return Err(SystemError::EINVAL);
                        }
                        if existing.member_count() >= existing.max_num_members() {
                            return Err(SystemError::ENOSPC);
                        }
                        existing.try_with_member(socket_ref.clone())?
                    }
                }
                None => {
                    writer
                        .by_id
                        .try_reserve(1)
                        .map_err(|_| SystemError::ENOMEM)?;
                    if writer
                        .id_alloc
                        .alloc_specific(params.id_req as usize)
                        .is_none()
                    {
                        return Err(SystemError::EINVAL);
                    }
                    reserved_new_id = Some(params.id_req);
                    match FanoutGroup::try_new(params.id_req, params, socket_ref.clone()) {
                        Ok(group) => group,
                        Err(err) => {
                            writer.id_alloc.free(params.id_req as usize);
                            return Err(err);
                        }
                    }
                }
            }
        };

        let prepared = match self.prepare_fanout_join_snapshot(
            &writer,
            &current,
            &socket_ref,
            group.clone(),
        ) {
            Ok(snapshot) => snapshot,
            Err(err) => {
                if let Some(id) = reserved_new_id {
                    writer.id_alloc.free(id as usize);
                }
                return Err(err);
            }
        };
        self.commit_packet_topology(prepared);
        writer.by_id.insert(group.id, group.clone());
        socket.set_fanout_membership(membership_value(&group));
        Ok(())
    }

    fn prepare_fanout_join_snapshot(
        &self,
        writer: &PacketSocketRegistryWriter,
        current: &PacketSocketRegistrySnapshot,
        socket: &Weak<PacketSocket>,
        replacement: Arc<FanoutGroup>,
    ) -> Result<Arc<PacketSocketRegistrySnapshot>, SystemError> {
        let mut sockets = Vec::new();
        sockets
            .try_reserve_exact(current.sockets.len())
            .map_err(|_| SystemError::ENOMEM)?;
        sockets.extend(
            current
                .sockets
                .iter()
                .filter(|entry| !Weak::ptr_eq(entry, socket))
                .cloned(),
        );

        let additional = usize::from(!writer.by_id.contains_key(&replacement.id));
        let mut groups = Vec::new();
        groups
            .try_reserve_exact(writer.by_id.len().saturating_add(additional))
            .map_err(|_| SystemError::ENOMEM)?;
        let mut replaced = false;
        for (id, group) in writer.by_id.iter() {
            if *id == replacement.id {
                groups.push(replacement.clone());
                replaced = true;
            } else {
                groups.push(group.clone());
            }
        }
        if !replaced {
            groups.push(replacement);
        }
        let live_receiver_count = sockets.len()
            + groups
                .iter()
                .map(|group| group.member_count())
                .sum::<usize>();
        Arc::try_new(PacketSocketRegistrySnapshot {
            sockets,
            groups,
            live_receiver_count,
        })
        .map_err(|_| SystemError::ENOMEM)
    }

    fn prepare_packet_topology_update(
        &self,
        writer: &PacketSocketRegistryWriter,
        sockets: Vec<Weak<PacketSocket>>,
        update: Option<&(u16, Arc<FanoutGroup>)>,
    ) -> Result<Arc<PacketSocketRegistrySnapshot>, SystemError> {
        let mut groups = Vec::new();
        groups
            .try_reserve_exact(writer.by_id.len())
            .map_err(|_| SystemError::ENOMEM)?;
        for (id, group) in writer.by_id.iter() {
            match update {
                Some((update_id, replacement)) if id == update_id => {
                    if replacement.member_count() != 0 {
                        groups.push(replacement.clone());
                    }
                }
                _ => groups.push(group.clone()),
            }
        }
        Self::try_packet_topology(sockets, groups)
    }

    fn try_packet_topology(
        sockets: Vec<Weak<PacketSocket>>,
        groups: Vec<Arc<FanoutGroup>>,
    ) -> Result<Arc<PacketSocketRegistrySnapshot>, SystemError> {
        let live_receiver_count = sockets.len()
            + groups
                .iter()
                .map(|group| group.member_count())
                .sum::<usize>();
        Arc::try_new(PacketSocketRegistrySnapshot {
            sockets,
            groups,
            live_receiver_count,
        })
        .map_err(|_| SystemError::ENOMEM)
    }

    fn commit_packet_topology(&self, snapshot: Arc<PacketSocketRegistrySnapshot>) {
        self.packet_sockets.store_deferred(snapshot);
    }

    #[inline]
    pub fn local_port_range(&self) -> (u16, u16) {
        let value = self.local_port_range.load(Ordering::Relaxed);
        ((value >> 16) as u16, (value & 0xffff) as u16)
    }

    pub fn set_local_port_range(&self, min: u16, max: u16) -> Result<(), SystemError> {
        if min == 0 || max == 0 || min > max {
            return Err(SystemError::EINVAL);
        }
        let new_value = ((min as u32) << 16) | (max as u32);
        loop {
            let old_value = self.local_port_range.load(Ordering::Relaxed);
            if old_value == new_value {
                return Ok(());
            }
            if self
                .local_port_range
                .compare_exchange(old_value, new_value, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
            {
                return Ok(());
            }
        }
    }

    pub fn ipv4_forwarding_value(&self) -> i32 {
        self.ipv4_forwarding.load(Ordering::Acquire)
    }

    pub fn ipv4_forwarding_enabled(&self) -> bool {
        self.ipv4_forwarding_value() != 0
    }

    pub fn set_ipv4_forwarding(&self, value: i32) {
        self.ipv4_forwarding.store(value, Ordering::Release);
    }

    pub fn ipv6_forwarding_value(&self) -> i32 {
        self.ipv6_forwarding.load(Ordering::Acquire)
    }

    pub fn ipv6_forwarding_enabled(&self) -> bool {
        self.ipv6_forwarding_value() != 0
    }

    pub fn set_ipv6_forwarding(&self, value: i32) {
        self.ipv6_forwarding.store(value, Ordering::Release);
    }

    pub fn inner(&self) -> RwLockReadGuard<'_, InnerNetNamespace> {
        self.inner.read()
    }

    pub fn inner_mut(&self) -> RwLockWriteGuard<'_, InnerNetNamespace> {
        self.inner.write()
    }

    pub fn set_loopback_iface(&self, loopback: Arc<LoopbackInterface>) {
        self.loopback_iface.store_deferred(Some(loopback));
    }

    pub fn loopback_iface(&self) -> Option<Arc<LoopbackInterface>> {
        self.loopback_iface.load()
    }

    pub fn set_default_iface(&self, iface: Arc<dyn Iface>) {
        self.default_iface
            .store_deferred(Some(DefaultIfaceRef::new(iface)));
    }

    pub fn default_iface(&self) -> Option<Arc<dyn Iface>> {
        self.default_iface
            .load()
            .map(|current| current.iface.clone())
    }

    pub fn router(&self) -> Arc<Router> {
        self.inner().router.clone()
    }

    pub fn netlink_socket_table(&self) -> &NetlinkSocketTable {
        &self.netlink_socket_table
    }

    pub(crate) fn nftables(&self) -> &NftNamespaceState {
        self.nftables
            .as_deref()
            .expect("live network namespace must own nftables state")
    }

    pub(crate) fn conntrack(&self) -> &CtState {
        &self.conntrack
    }

    /// Prepare the bounded IPv4 ingress and local-loopback fragment handoff
    /// before publishing conntrack-dependent rules. This may allocate and
    /// must be called in the nft transaction's process context. On failure no
    /// rule may be published and ordinary fragment handling stays unchanged.
    pub(crate) fn prepare_ipv4_defrag(&self) -> Result<(), SystemError> {
        self.poller.prepare_ip_defrag()
    }

    /// Call only after successful preparation and conntrack activation, and
    /// before publishing rules that require CT/NAT. Until then the packet path
    /// retains its stateless-fragment behavior.
    pub(crate) fn enable_ipv4_defrag(&self) -> Result<(), SystemError> {
        self.poller.enable_ipv4_defrag()
    }

    pub(crate) fn prepare_ipv6_defrag(&self) -> Result<(), SystemError> {
        // Both address families share one bounded, preallocated handoff ring.
        self.poller.prepare_ip_defrag()
    }

    pub(crate) fn enable_ipv6_defrag(&self) -> Result<(), SystemError> {
        self.poller.enable_ipv6_defrag()
    }

    pub(crate) fn ipv4_defrag_enabled(&self) -> bool {
        self.poller.defrag_enabled.load(Ordering::Acquire)
    }

    pub(crate) fn ipv6_defrag_enabled(&self) -> bool {
        self.poller.ipv6_defrag_enabled.load(Ordering::Acquire)
    }

    pub(crate) fn queue_ip_fragment(
        &self,
        fragment: PendingIpFragment,
    ) -> Result<(), PendingIpFragment> {
        self.poller.queue_ip_fragment(fragment)
    }

    /// Confirmation may establish a new GC deadline. Wake the namespace
    /// scheduler after releasing the conntrack lock, never from inside it.
    pub(crate) fn confirm_conntrack(
        &self,
        candidate: CtCandidate,
        now: Instant,
    ) -> Result<CtConfirm, CtError> {
        let confirmed = self.conntrack.confirm(candidate, now)?;
        if matches!(confirmed, CtConfirm::Inserted(_)) {
            self.notify_deadline_changed();
        }
        Ok(confirmed)
    }

    pub fn unix_abstract_table(&self) -> &Arc<UnixAbstractTable> {
        &self.unix_abstract_table
    }

    pub fn get_netlink_kernel_socket_by_protocol(
        &self,
        protocol: u32,
    ) -> Option<Arc<dyn NetlinkKernelSocket>> {
        self.netlink_kernel_socket.read().get(&protocol).cloned()
    }

    pub fn add_device(&self, device: Arc<dyn Iface>) -> Result<(), SystemError> {
        let rtnl = crate::net::rtnl::lock();
        self.add_device_locked(&rtnl, device)
    }

    /// Register a device while the caller already owns RTNL (rtnetlink doit).
    /// Keeping the outer guard avoids recursively acquiring the non-reentrant
    /// control-plane lock during dynamic netdevice creation.
    pub(crate) fn add_device_locked(
        &self,
        rtnl: &crate::net::rtnl::RtnlGuard,
        device: Arc<dyn Iface>,
    ) -> Result<(), SystemError> {
        // Keep topology readers behind this write guard until both the map and
        // the authoritative FIB/projections contain the new interface.
        let mut devices = self.device_list_mut();
        if devices.contains_key(&device.nic_id()) {
            return Err(SystemError::EEXIST);
        }
        if device.net_namespace().is_some() {
            return Err(SystemError::EBUSY);
        }
        let requested_name = device.iface_name();
        if devices.values().any(|existing| {
            existing
                .common()
                .with_iface_name(|name| name == requested_name)
        }) {
            return Err(SystemError::EEXIST);
        }
        // Build every fallible transaction input while the device is still
        // unpublished. The write guard keeps the topology stable, and the new
        // interface is appended explicitly for projection preparation.
        let participants = try_snapshot_devices(&devices, Some(&device))?;
        let netns = self.self_ref.upgrade().unwrap();
        device.set_net_namespace(netns.clone())?;
        let broadcast_count = device.common().configured_ipv4_broadcast_count();
        self.publish_ipv4_broadcast_count_change(0, broadcast_count);
        devices.insert(device.nic_id(), device.clone());
        let iface = device.clone();
        if let Err(error) = crate::net::route::register_iface(rtnl, &netns, &iface, &participants) {
            devices.remove(&device.nic_id());
            self.publish_ipv4_broadcast_count_change(broadcast_count, 0);
            device.clear_net_namespace();
            return Err(error);
        }
        drop(devices);
        self.notify_deadline_changed();

        // log::info!(
        //     "Network device added to namespace count: {:?}",
        //     self.device_list().len()
        // );
        Ok(())
    }

    /// Prepare the topology half of publishing one or two route-free software
    /// interfaces. RTNL stabilizes names and indices until the owned candidate
    /// map is swapped in; all ingress allocations are done before that swap.
    pub(crate) fn prepare_add_devices_locked(
        &self,
        _rtnl: &crate::net::rtnl::RtnlGuard,
        additions: &[(Arc<dyn Iface>, &str)],
    ) -> Result<PreparedNetnsDeviceAddition, SystemError> {
        let devices = self.device_list();
        let mut candidate = devices.clone();
        let mut names = Vec::new();
        let mut bindings = Vec::new();
        names
            .try_reserve_exact(additions.len())
            .map_err(|_| SystemError::ENOMEM)?;
        bindings
            .try_reserve_exact(additions.len())
            .map_err(|_| SystemError::ENOMEM)?;
        let netns = self.self_ref.upgrade().ok_or(SystemError::ENODEV)?;
        for (iface, name) in additions {
            if candidate.contains_key(&iface.nic_id())
                || names.contains(name)
                || devices.values().any(|existing| {
                    existing
                        .common()
                        .with_iface_name(|current| current == *name)
                })
            {
                return Err(SystemError::EEXIST);
            }
            names.push(*name);
            let binding =
                crate::driver::net::PreparedNetnsBinding::prepare(&netns, iface.nic_id())?;
            bindings.push((iface.clone(), binding));
            candidate.insert(iface.nic_id(), iface.clone());
        }
        Ok(PreparedNetnsDeviceAddition {
            netns,
            candidate,
            bindings,
        })
    }

    pub fn remove_device(&self, nic_id: &usize) {
        // Teardown helper only: the caller must quiesce IRQ, DMA, and NAPI
        // before removing an active device. Runtime hot-remove is not provided
        // by this API.
        let rtnl = crate::net::rtnl::lock();
        match self.remove_device_locked(&rtnl, nic_id) {
            Ok(device) => device.clear_net_namespace(),
            Err(error) => log::error!("failed to remove interface {}: {:?}", nic_id, error),
        }
    }

    /// Topology/FIB removal under the caller's RTNL guard. The caller must
    /// quiesce ingress, NAPI and TX before invoking this operation.
    pub(crate) fn remove_device_locked(
        &self,
        rtnl: &crate::net::rtnl::RtnlGuard,
        nic_id: &usize,
    ) -> Result<Arc<dyn Iface>, SystemError> {
        let removed = self
            .device_list()
            .get(nic_id)
            .cloned()
            .ok_or(SystemError::ENODEV)?;
        self.prepare_remove_devices_locked(rtnl, core::slice::from_ref(&removed))?
            .publish();
        Ok(removed)
    }

    pub(crate) fn prepare_remove_devices_locked<'rtnl>(
        &self,
        rtnl: &'rtnl crate::net::rtnl::RtnlGuard,
        removed: &[Arc<dyn Iface>],
    ) -> Result<PreparedNetnsDeviceRemoval<'rtnl>, SystemError> {
        self.prepare_remove_devices_from_locked(rtnl, removed, None)
    }

    pub(crate) fn prepare_remove_devices_from_locked<'rtnl>(
        &self,
        rtnl: &'rtnl crate::net::rtnl::RtnlGuard,
        removed: &[Arc<dyn Iface>],
        staged_before: Option<crate::net::link::StagedLinkFib<'_>>,
    ) -> Result<PreparedNetnsDeviceRemoval<'rtnl>, SystemError> {
        let devices = self.device_list();
        let mut ifindices = Vec::new();
        let mut route_indices = Vec::new();
        ifindices
            .try_reserve_exact(removed.len())
            .map_err(|_| SystemError::ENOMEM)?;
        route_indices
            .try_reserve_exact(removed.len())
            .map_err(|_| SystemError::ENOMEM)?;
        for iface in removed {
            let ifindex = iface.nic_id();
            if ifindices.contains(&ifindex)
                || !devices
                    .get(&ifindex)
                    .is_some_and(|current| Arc::ptr_eq(current, iface))
            {
                return Err(SystemError::ENODEV);
            }
            ifindices.push(ifindex);
            route_indices.push(u32::try_from(ifindex).map_err(|_| SystemError::EINVAL)?);
        }
        let participants = try_snapshot_devices(&devices, None)?;
        drop(devices);
        let netns = self.self_ref.upgrade().ok_or(SystemError::ENODEV)?;
        let routes = crate::net::route::prepare_unregister_ifaces_from(
            rtnl,
            &netns,
            &route_indices,
            &participants,
            staged_before,
        )?;
        Ok(PreparedNetnsDeviceRemoval {
            rtnl,
            netns,
            ifindices,
            routes,
        })
    }

    /// Roll back a newly created, unannounced software link after its caller
    /// has verified that no authoritative route references it. This avoids a
    /// second fallible FIB snapshot merely to undo a route-free registration.
    pub(crate) fn remove_unrouted_devices_locked(
        &self,
        rtnl: &crate::net::rtnl::RtnlGuard,
        removed: &[Arc<dyn Iface>],
    ) {
        let mut devices = self.device_list_mut();
        let removed_broadcasts: usize = removed
            .iter()
            .map(|iface| iface.common().configured_ipv4_broadcast_count())
            .sum();
        for iface in removed {
            let ifindex = iface.nic_id();
            assert!(
                devices
                    .get(&ifindex)
                    .is_some_and(|current| Arc::ptr_eq(current, iface)),
                "RTNL must retain the just-created device until rollback"
            );
            devices.remove(&ifindex);
        }
        self.publish_ipv4_broadcast_count_change(removed_broadcasts, 0);
        drop(devices);
        for iface in removed {
            self.finish_removed_devices_locked(rtnl, core::slice::from_ref(&iface.nic_id()));
        }
    }

    fn finish_removed_devices_locked(
        &self,
        rtnl: &crate::net::rtnl::RtnlGuard,
        ifindices: &[usize],
    ) {
        let netns = self
            .self_ref
            .upgrade()
            .expect("registered netns remains live under RTNL");
        for &ifindex in ifindices {
            crate::net::neighbor::remove_iface(rtnl, &netns, ifindex as u32);
            self.conntrack().invalidate_masquerade_oif(ifindex as u32);
            self.default_iface
                .clear_if_deferred(|current| current.iface.nic_id() == ifindex);
            self.loopback_iface
                .clear_if_deferred(|current| current.nic_id() == ifindex);
        }
        self.notify_deadline_changed();
    }

    pub fn insert_bridge(&self, bridge: Arc<BridgeDriver>) {
        self.bridge_list.write().insert(bridge.name(), bridge);
    }

    /// # 拉起网络命名空间的轮询线程
    /// 设置 poll_pending 标志并唤醒等待队列中的线程
    /// 使用原子标志确保即使 poll 线程正在执行也不会丢失唤醒请求
    pub fn wakeup_poll_thread(&self) {
        // 先设置 pending 标志，再唤醒：避免“先唤后睡/睡前漏信号”。
        let (newly_pending, woken) = self.poller.notify_network();
        // 事件驱动：对齐 Linux，尽量在事件发生后立刻 schedule NAPI（由 NAPI 线程 bounded poll 推进）。
        // 只在从“未 pending -> pending”这一跳触发一次，避免中断风暴下重复 schedule。
        if newly_pending {
            for (_, iface) in self.device_list.read().iter() {
                if let Some(napi) = iface.napi_struct() {
                    napi_schedule(napi);
                }
            }
            log::trace!("netns: wakeup_poll_thread: woken={}", woken);
        }
    }

    /// Request a deadline-only timeout rescan without treating it as immediate
    /// network I/O. Safe to call after dropping smoltcp and topology locks.
    pub fn notify_deadline_changed(&self) {
        self.poller.notify_deadline_changed();
    }

    fn create_polling_thread(netns: Arc<Self>, name: String) {
        netns.poller.start(name);
    }
}

impl NamespaceOps for NetNamespace {
    fn ns_common(&self) -> &NsCommon {
        &self.ns_common
    }
}

impl Drop for NetNamespace {
    fn drop(&mut self) {
        self.poller.stop();

        if let Some(loopback) = self.loopback_iface() {
            let loopback_iface = loopback as Arc<dyn Iface>;
            if !self
                .device_list
                .get_mut()
                .get(&crate::net::LOOPBACK_IFINDEX)
                .is_some_and(|device| Arc::ptr_eq(device, &loopback_iface))
            {
                log::error!(
                    "netns {} final release contains a non-canonical loopback",
                    self.ns_common.nsid.data()
                );
            }
        }

        // The last netns Arc may be released by its own NAPI poll stack. Move
        // the devices into the preallocated work payload and enqueue it without
        // allocating. Every blocking teardown step then runs in process context,
        // where waiting for the in-flight poll cannot self-deadlock. The
        // namespace-owned FIB and neighbor state disappear with this object, so
        // the worker only quiesces devices and removes driver-core projections.
        let devices = core::mem::take(self.device_list.get_mut());
        let nftables = self
            .nftables
            .take()
            .expect("network namespace must own nftables state until final release");
        // A live conntrack table may own thousands of flow allocations. Its
        // final release must not run on the NAPI stack that drops this netns.
        let conntrack = core::mem::replace(&mut self.conntrack, CtState::new());
        self.teardown_work.enqueue(NetnsTeardownPayload {
            devices,
            nftables,
            conntrack,
        });
    }
}

impl ProcessManager {
    pub fn current_netns() -> Arc<NetNamespace> {
        Self::current_pcb().nsproxy().net_ns.clone()
    }
}
