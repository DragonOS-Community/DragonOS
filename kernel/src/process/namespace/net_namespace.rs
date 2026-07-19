use crate::driver::net::bridge::BridgeDriver;
use crate::driver::net::loopback::LoopbackInterface;
use crate::init::initcall::INITCALL_SUBSYS;
use crate::libs::mutex::Mutex;
use crate::libs::rwlock::{RwLock, RwLockReadGuard, RwLockWriteGuard};
use crate::libs::rwsem::{RwSem, RwSemReadGuard, RwSemWriteGuard};
use crate::libs::wait_queue::WaitQueue;
use crate::net::routing::Router;
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
    driver::net::napi::napi_schedule,
    driver::net::Iface,
    process::namespace::{nsproxy::NsCommon, user_namespace::UserNamespace},
};
use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::AtomicU32;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use hashbrown::HashMap;
use ida::IdAllocator;
use system_error::SystemError;
use unified_init::macros::unified_init;

lazy_static! {
    /// # 所有网络设备，进程，socket的初始网络命名空间
    pub static ref INIT_NET_NAMESPACE: Arc<NetNamespace> = NetNamespace::new_root();
}

/// # 网络命名空间计数器
/// 用于生成唯一的网络命名空间ID
/// 每次创建新的网络命名空间时，都会增加这个计数器
pub static mut NETNS_COUNTER: AtomicUsize = AtomicUsize::new(0);

const PACKET_SOCKET_CLEANUP_RETRY_MIN: Duration = Duration::from_millis(100);
const PACKET_SOCKET_CLEANUP_RETRY_MAX: Duration = Duration::from_secs(5);

#[unified_init(INITCALL_SUBSYS)]
pub fn root_net_namespace_init() -> Result<(), SystemError> {
    // 创建root网络命名空间的轮询线程
    NetNamespace::create_polling_thread(INIT_NET_NAMESPACE.clone(), "root_netns".to_string());

    // 创建 router
    let router = Router::new("root_netns_router".to_string());
    INIT_NET_NAMESPACE.inner_mut().router = router.clone();
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
    device_list: RwSem<BTreeMap<usize, Arc<dyn Iface>>>,
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
    /// # 当前网络命名空间下的 Netlink 内核套接字
    /// 负责接收并处理 Netlink 消息
    netlink_kernel_socket: RwSem<HashMap<u32, Arc<dyn NetlinkKernelSocket>>>,

    /// AF_UNIX abstract namespace table (scoped to this netns).
    unix_abstract_table: Arc<UnixAbstractTable>,
    /// Per-netns IPv4 ephemeral port range (ip_local_port_range)
    local_port_range: AtomicU32,
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
            thread: RwSem::new(None),
        })
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

    fn polling(&self) {
        let mut cleanup_retry_delay = PACKET_SOCKET_CLEANUP_RETRY_MIN;
        let mut cleanup_retry_at = None;
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
            let now_us = Instant::now().total_micros() as u64;
            if cleanup_retry_at.is_none_or(|deadline| now_us >= deadline) {
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
                            Some(now_us.saturating_add(cleanup_retry_delay.total_micros()));
                        cleanup_retry_delay = Duration::from_micros(core::cmp::min(
                            cleanup_retry_delay.total_micros().saturating_mul(2),
                            PACKET_SOCKET_CLEANUP_RETRY_MAX.total_micros(),
                        ));
                    }
                }
            }

            // 处理“已到期的定时事件”：到期则 schedule NAPI 推进一次。
            // 同时计算下一次最早到期时间点，用于设置 sleep 超时。
            let mut next_us = cleanup_retry_at;
            let mut had_due = false;
            for (_, iface) in netns.device_list.read().iter() {
                if let Some(us) = iface.common().poll_at_us() {
                    if us <= now_us {
                        had_due = true;
                        if let Some(napi) = iface.napi_struct() {
                            napi_schedule(napi);
                        } else {
                            // 兜底：若未配置 NAPI，则仍调用一次 poll 推进（可能无界）。
                            let _ = iface.poll();
                        }
                        continue;
                    }

                    next_us = Some(match next_us {
                        Some(cur) => core::cmp::min(cur, us),
                        None => us,
                    });
                }
            }

            // sleep 超时：
            // - 若刚处理了 due timer：小睡一会儿，避免在 NAPI 尚未推进/更新时间戳前重复 schedule 形成忙等
            // - 否则按最早 deadline 精确睡眠
            let timeout = if had_due {
                Some(Duration::from_micros(200))
            } else {
                next_us.map(|us| {
                    let delta = us.saturating_sub(now_us);
                    Duration::from_micros(core::cmp::max(1, delta))
                })
            };

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
            let woke_by_event = match self.wait_queue.wait_event_uninterruptible_timeout(
                || {
                    self.poll_pending.load(Ordering::Acquire)
                        || self.cleanup_pending.load(Ordering::Acquire)
                },
                timeout,
            ) {
                Ok(()) => true,
                Err(SystemError::EAGAIN_OR_EWOULDBLOCK) => false,
                Err(e) => {
                    log::warn!("netns scheduler sleep error: {:?}", e);
                    false
                }
            };

            if woke_by_event {
                let network_pending = self.poll_pending.swap(false, Ordering::AcqRel);
                self.cleanup_pending.store(false, Ordering::Release);
                if KernelThreadMechanism::should_stop(&ProcessManager::current_pcb()) {
                    break;
                }
                if !network_pending {
                    continue;
                }
                let netns = match self.netns.upgrade() {
                    Some(netns) => netns,
                    None => {
                        break;
                    }
                };

                // 事件驱动：尽量只 schedule 一次即可，由 NAPI 线程以 bounded poll 推进。
                for (_, iface) in netns.device_list.read().iter() {
                    if let Some(napi) = iface.napi_struct() {
                        napi_schedule(napi);
                    } else {
                        let _ = iface.poll();
                    }
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
    pub fn new_root() -> Arc<Self> {
        let inner = InnerNetNamespace {
            // 这里没有直接创建 router，而是留到 init 函数中创建
            router: Router::new_empty(),
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
            packet_sockets: RcuArcSlot::new(Arc::new(PacketSocketRegistrySnapshot::default())),
            packet_sockets_writer: Mutex::new(PacketSocketRegistryWriter::new()),
            packet_sockets_need_cleanup: AtomicBool::new(false),
            bridge_list: RwSem::new(BTreeMap::new()),
            netlink_socket_table: NetlinkSocketTable::default(),
            netlink_kernel_socket: RwSem::new(generate_supported_netlink_kernel_sockets()),
            unix_abstract_table: unix_abstract_table.clone(),
            local_port_range: AtomicU32::new(
                crate::net::socket::inet::common::port::DEFAULT_LOCAL_PORT_RANGE,
            ),
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
            1,
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
            packet_sockets: RcuArcSlot::new(Arc::new(PacketSocketRegistrySnapshot::default())),
            packet_sockets_writer: Mutex::new(PacketSocketRegistryWriter::new()),
            packet_sockets_need_cleanup: AtomicBool::new(false),
            bridge_list: RwSem::new(BTreeMap::new()),
            netlink_socket_table: NetlinkSocketTable::default(),
            netlink_kernel_socket: RwSem::new(generate_supported_netlink_kernel_sockets()),
            unix_abstract_table: unix_abstract_table.clone(),
            local_port_range: AtomicU32::new(
                crate::net::socket::inet::common::port::DEFAULT_LOCAL_PORT_RANGE,
            ),
            loopback_iface: RcuOptionArcSlot::new_some(loopback.clone()),
            default_iface: RcuOptionArcSlot::new_none(),
        });

        // Linux 语义：每个 netns 都需要一个可被唤醒的轮询线程来推进协议栈。
        // 否则像 lo 这样的设备在 Tx 后仅通过 wakeup_poll_thread() 触发下一次 poll，
        // 若此处不记录 pcb，后续将无法唤醒，从而导致 TCP connect/accept 等卡死。
        Self::create_polling_thread(netns.clone(), format!("netns_{}", counter));
        netns.add_device(loopback);

        Ok(netns)
    }

    pub fn user_ns(&self) -> &Arc<UserNamespace> {
        &self._user_ns
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

    pub fn unix_abstract_table(&self) -> &Arc<UnixAbstractTable> {
        &self.unix_abstract_table
    }

    pub fn get_netlink_kernel_socket_by_protocol(
        &self,
        protocol: u32,
    ) -> Option<Arc<dyn NetlinkKernelSocket>> {
        self.netlink_kernel_socket.read().get(&protocol).cloned()
    }

    pub fn add_device(&self, device: Arc<dyn Iface>) {
        device.set_net_namespace(self.self_ref.upgrade().unwrap());

        self.device_list_mut().insert(device.nic_id(), device);

        // log::info!(
        //     "Network device added to namespace count: {:?}",
        //     self.device_list().len()
        // );
    }

    pub fn remove_device(&self, nic_id: &usize) {
        let removed = self.device_list_mut().remove(nic_id);
        if removed.is_none() {
            return;
        }

        self.default_iface
            .clear_if_deferred(|current| current.iface.nic_id() == *nic_id);
        self.loopback_iface
            .clear_if_deferred(|current| current.nic_id() == *nic_id);
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
    }
}

impl ProcessManager {
    pub fn current_netns() -> Arc<NetNamespace> {
        Self::current_pcb().nsproxy().net_ns.clone()
    }
}
