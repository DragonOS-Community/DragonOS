use alloc::collections::VecDeque;
use alloc::ffi::CString;
use alloc::sync::Weak;
use alloc::{fmt, vec::Vec};
use alloc::{string::String, sync::Arc};
use core::net::Ipv4Addr;
use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use net_poll_state::{DueResult, PollDeadline, PublishResult};
use sysfs::netdev_register_kobject;

use crate::driver::net::napi::NapiStruct;
use crate::driver::net::types::{InterfaceFlags, InterfaceType};
use crate::libs::rwsem::{RwSem, RwSemReadGuard};
use crate::libs::spinlock::SpinLock;
use crate::net::routing::RouterEnableDeviceCommon;
use crate::process::namespace::net_namespace::NetNamespace;
use crate::{
    libs::{mutex::Mutex, rwlock::RwLock},
    net::socket::inet::{common::PortManager, InetSocket},
    process::ProcessState,
};
use smoltcp::phy::{Device as SmolDevice, DeviceCapabilities, RxToken};
use system_error::SystemError;

pub mod bridge;
pub mod class;
mod dma;
pub mod e1000e;
pub mod loopback;
pub mod napi;
pub mod sysfs;
pub mod types;
pub mod veth;
pub mod virtio_net;

kernel_cmdline_param_arg!(
    NET_TEST_FIXTURES_PARAM,
    "dragonos.net_test_fixtures",
    false,
    false
);

/// Whether boot-only networking fixtures should be installed in the initial
/// network namespace. Production boots leave this disabled; test runners opt
/// in explicitly so synthetic veth and bridge devices cannot pollute the
/// system route table or device namespace.
pub(crate) fn net_test_fixtures_enabled() -> bool {
    NET_TEST_FIXTURES_PARAM.value_bool().unwrap_or(false)
}

bitflags! {
    pub struct NetDeivceState: u16 {
        /// 表示网络设备已经启动
        const __LINK_STATE_START = 1 << 0;
        /// 表示网络设备在系统中存在，即注册到sysfs中
        const __LINK_STATE_PRESENT = 1 << 1;
        /// 表示网络设备没有检测到载波信号
        const __LINK_STATE_NOCARRIER = 1 << 2;
        /// 表示设备的链路监视操作处于挂起状态
        const __LINK_STATE_LINKWATCH_PENDING = 1 << 3;
        /// 表示设备处于休眠状态
        const __LINK_STATE_DORMANT = 1 << 4;
    }
}

#[derive(Debug, Copy, Clone)]
#[allow(dead_code, non_camel_case_types)]
pub enum Operstate {
    /// 网络接口的状态未知
    IF_OPER_UNKNOWN = 0,
    /// 网络接口不存在
    IF_OPER_NOTPRESENT = 1,
    /// 网络接口已禁用或未连接
    IF_OPER_DOWN = 2,
    /// 网络接口的下层接口已关闭
    IF_OPER_LOWERLAYERDOWN = 3,
    /// 网络接口正在测试
    IF_OPER_TESTING = 4,
    /// 网络接口处于休眠状态
    IF_OPER_DORMANT = 5,
    /// 网络接口已启用
    IF_OPER_UP = 6,
}

/// Control-plane metadata attached to one configured interface address.
///
/// `label == None` means the interface's current primary name. Keeping that
/// distinction avoids stale labels after a device rename while preserving an
/// explicitly supplied Linux IFA_LABEL alias verbatim.
#[derive(Debug, Clone)]
pub(crate) struct AddressMetadata {
    pub cidr: smoltcp::wire::IpCidr,
    pub label: Option<CString>,
}

/// Lossless route state accumulated while an interface is being constructed.
///
/// This is only a hand-off buffer: once the interface is published, its
/// network namespace FIB owns the route and the smoltcp table is a projection
/// of that FIB.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BootstrapRoute {
    pub destination: smoltcp::wire::IpCidr,
    pub source: Option<smoltcp::wire::IpCidr>,
    pub preferred_source: Option<smoltcp::wire::IpAddress>,
    pub table: u32,
    pub priority: u32,
    pub tos: u8,
    pub protocol: u8,
    pub scope: u8,
    pub kind: u8,
    pub oif: u32,
    pub gateway: Option<smoltcp::wire::IpAddress>,
    pub nexthop_flags: u8,
}

#[allow(dead_code)]
pub trait Iface: crate::driver::base::device::Device {
    /// # `common`
    /// 获取网卡的公共信息
    fn common(&self) -> &IfaceCommon;

    /// # `mac`
    /// 获取网卡的MAC地址
    ///
    /// This method is used from the RX data path while the caller may already
    /// hold [`IfaceCommon::smol_iface`]. Implementations must therefore read
    /// device metadata directly and must not acquire `smol_iface` again.
    fn mac(&self) -> smoltcp::wire::EthernetAddress;

    /// # `name`
    /// 获取网卡名
    fn iface_name(&self) -> String;

    /// # `nic_id`
    /// 获取网卡id
    fn nic_id(&self) -> usize {
        self.common().iface_id
    }

    /// # `poll`
    /// 用于轮询网卡，处理网络事件
    /// ## 返回值
    /// - `true`：表示有网络事件发生
    /// - `false`：表示没有网络事件
    fn poll(&self) -> bool;

    /// # `poll_napi`
    /// NAPI（类似 Linux softirq/ksoftirqd）使用的 bounded poll。
    ///
    /// 返回本次实际处理量，以及是否需要立即再次 poll。
    fn poll_napi(&self, budget: usize) -> napi::NapiPollResult;

    /// Called after this NAPI instance acquires `SCHED`, before it is published.
    /// Devices with interrupt mitigation may mask callbacks here.
    #[inline]
    fn napi_poll_begin(&self) {}

    /// Complete one NAPI ownership cycle. Devices may override this to pair
    /// callback re-enabling with the `SCHED/MISSED` transition.
    #[inline]
    fn napi_complete(&self, napi: Arc<napi::NapiStruct>) {
        napi::napi_complete(napi);
    }

    /// # `raw_transmit`
    /// Send a raw Ethernet frame (for AF_PACKET).
    ///
    /// By default returns `ENOSYS`; concrete NIC drivers should override this
    /// method to send frames directly through the underlying `phy::Device`
    /// TX channel, bypassing the smoltcp stack.
    fn raw_transmit(&self, _frame: &[u8]) -> Result<(), SystemError> {
        Err(SystemError::ENOSYS)
    }

    /// Sends an already-routed IP packet through this interface. Only devices
    /// that participate in DragonOS software forwarding need to override it.
    fn route_and_send(
        &self,
        _next_hop: &smoltcp::wire::IpAddress,
        _ip_packet: &[u8],
    ) -> Result<(), SystemError> {
        Err(SystemError::EOPNOTSUPP_OR_ENOTSUP)
    }

    /// Hands a namespace-local IPv4 packet to this interface's protocol stack
    /// without emitting it on the link. The shared queue in `IfaceCommon`
    /// makes local delivery a protocol-stack capability rather than an
    /// optional device-driver feature.
    fn inject_local_ipv4_packet(
        &self,
        source_mac: smoltcp::wire::EthernetAddress,
        ip_packet: &[u8],
        broadcast: bool,
    ) -> Result<(), SystemError> {
        let packet = LocalInputPacket {
            destination_mac: if broadcast {
                smoltcp::wire::EthernetAddress::BROADCAST
            } else {
                self.mac()
            },
            source_mac,
            ip_packet: ip_packet.to_vec(),
        };

        let napi = self.napi_struct();
        let netns = napi.is_none().then(|| self.net_namespace()).flatten();
        if napi.is_none() && netns.is_none() {
            return Err(SystemError::ENODEV);
        }
        self.common().enqueue_local_input(packet)?;
        if let Some(napi) = napi {
            napi::napi_schedule(napi);
        } else if let Some(netns) = netns {
            netns.wakeup_poll_thread();
        }
        Ok(())
    }

    /// # `should_drop_rx_packet`
    /// 驱动收包入口可选调用：询问“上层(协议栈/Socket 语义)”是否需要丢弃该包。
    ///
    /// 说明：
    /// - 默认不丢弃；
    /// - 驱动层不应理解 TCP/UDP 等协议语义，这个 hook 用于实现 Linux 兼容语义（如 backlog 满丢 SYN）
    ///   且不修改 smoltcp。
    #[inline]
    fn should_drop_rx_packet(&self, _packet: &[u8]) -> bool {
        false
    }

    /// @brief 获取smoltcp的网卡接口类型
    #[inline(always)]
    fn smol_iface(&self) -> &Mutex<smoltcp::iface::Interface> {
        &self.common().smol_iface
    }
    // fn as_any_ref(&'static self) -> &'static dyn core::any::Any;

    /// # `sockets`
    /// 获取网卡的套接字集
    fn sockets(&self) -> &Mutex<smoltcp::iface::SocketSet<'static>> {
        &self.common().sockets
    }

    /// # `port_manager`
    /// 用于管理网卡的端口
    fn port_manager(&self) -> &PortManager {
        &self.common().port_manager
    }

    fn addr_assign_type(&self) -> u8;

    fn net_device_type(&self) -> u16;

    fn net_state(&self) -> NetDeivceState;

    fn set_net_state(&self, state: NetDeivceState);

    /// Clear lifecycle state bits previously set with `set_net_state`.
    fn clear_net_state(&self, state: NetDeivceState);

    fn operstate(&self) -> Operstate;

    fn set_operstate(&self, state: Operstate);

    fn net_namespace(&self) -> Option<Arc<NetNamespace>> {
        self.common().net_namespace()
    }

    fn set_net_namespace(&self, ns: Arc<NetNamespace>) {
        self.common().set_net_namespace(ns);
    }

    fn clear_net_namespace(&self) {
        self.common().clear_net_namespace();
    }

    fn flags(&self) -> InterfaceFlags {
        self.common().flags()
    }

    /// Flags exported through Linux network-device user APIs.
    ///
    /// Like Linux `dev_get_flags()`, runtime flags are derived from the
    /// device lifecycle, operational state, and carrier state instead of
    /// exposing their cached initialization values.
    fn user_visible_flags(&self) -> InterfaceFlags {
        self.project_user_visible_flags(self.common().configured_flags())
    }

    /// Project one configured-flags snapshot into the Linux userspace view.
    fn project_user_visible_flags(&self, mut flags: InterfaceFlags) -> InterfaceFlags {
        flags.remove(InterfaceFlags::RUNNING | InterfaceFlags::LOWER_UP | InterfaceFlags::DORMANT);

        let state = self.net_state();
        if !state.contains(NetDeivceState::__LINK_STATE_START) {
            return flags;
        }

        if matches!(
            self.operstate(),
            Operstate::IF_OPER_UP | Operstate::IF_OPER_UNKNOWN
        ) {
            flags.insert(InterfaceFlags::RUNNING);
        }
        if !state.contains(NetDeivceState::__LINK_STATE_NOCARRIER) {
            flags.insert(InterfaceFlags::LOWER_UP);
        }
        if state.contains(NetDeivceState::__LINK_STATE_DORMANT) {
            flags.insert(InterfaceFlags::DORMANT);
        }
        flags
    }

    fn type_(&self) -> InterfaceType {
        self.common().type_()
    }

    fn mtu(&self) -> usize;

    /// # 获取当前iface的napi结构体
    /// 默认返回None，表示不支持napi
    fn napi_struct(&self) -> Option<Arc<napi::NapiStruct>> {
        self.common().napi_struct.read().clone()
    }

    fn router_common(&self) -> &RouterEnableDeviceCommon {
        &self.common().router_common_data
    }
}

/// 网络设备的公共数据
#[derive(Debug)]
pub struct NetDeviceCommonData {
    /// 表示网络接口的地址分配类型
    pub addr_assign_type: u8,
    /// 表示网络接口的类型
    pub net_device_type: u16,
    /// 表示网络接口的状态
    pub state: NetDeivceState,
    /// 表示网络接口的操作状态
    pub operstate: Operstate,
}

impl Default for NetDeviceCommonData {
    fn default() -> Self {
        Self {
            addr_assign_type: 0,
            net_device_type: 1,
            state: NetDeivceState::empty(),
            operstate: Operstate::IF_OPER_UNKNOWN,
        }
    }
}

#[derive(Debug, Clone)]
pub struct StaticNeighborEntry {
    pub ip_addr: smoltcp::wire::IpAddress,
    pub hw_addr: smoltcp::wire::HardwareAddress,
    pub state: u16,
    pub flags: u8,
}

/// 将网络设备注册到sysfs中
/// 参考：https://code.dragonos.org.cn/xref/linux-2.6.39/net/core/dev.c?fi=register_netdev#5373
fn register_netdevice(dev: Arc<dyn Iface>) -> Result<(), SystemError> {
    // 在sysfs中注册设备
    netdev_register_kobject(dev.clone())?;

    // 标识网络设备在系统中存在
    dev.set_net_state(NetDeivceState::__LINK_STATE_PRESENT);

    return Ok(());
}

#[derive(Debug)]
struct ReceiveModeState {
    configured_flags: u32,
    packet_promiscuity: u32,
    packet_allmulti: u32,
}

#[derive(Debug, Clone, Copy)]
pub struct LinkFlagsSnapshot {
    pub configured: InterfaceFlags,
    pub promiscuity: u32,
    pub allmulti: u32,
}

struct LocalInputRxToken {
    frame: Vec<u8>,
}

impl RxToken for LocalInputRxToken {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        f(&self.frame)
    }
}

#[derive(Debug)]
struct LocalInputPacket {
    destination_mac: smoltcp::wire::EthernetAddress,
    source_mac: smoltcp::wire::EthernetAddress,
    ip_packet: Vec<u8>,
}

impl LocalInputPacket {
    fn len(&self) -> usize {
        self.ip_packet.len()
    }

    fn into_frame(self, medium: smoltcp::phy::Medium) -> Vec<u8> {
        if medium == smoltcp::phy::Medium::Ip {
            return self.ip_packet;
        }
        let mut frame = Vec::with_capacity(14 + self.ip_packet.len());
        frame.extend_from_slice(&self.destination_mac.0);
        frame.extend_from_slice(&self.source_mac.0);
        frame.extend_from_slice(&[0x08, 0x00]);
        frame.extend_from_slice(&self.ip_packet);
        frame
    }
}

#[derive(Debug)]
struct LocalInputQueueState {
    packets: VecDeque<LocalInputPacket>,
    bytes: usize,
    accepting: bool,
}

#[derive(Debug)]
struct LocalInputQueue {
    state: SpinLock<LocalInputQueueState>,
}

impl LocalInputQueue {
    const MAX_FRAMES: usize = 1024;
    const MAX_BYTES: usize = 4 * 1024 * 1024;

    fn new(accepting: bool) -> Self {
        Self {
            state: SpinLock::new(LocalInputQueueState {
                packets: VecDeque::new(),
                bytes: 0,
                accepting,
            }),
        }
    }

    fn enqueue(&self, packet: LocalInputPacket) -> Result<(), SystemError> {
        let mut state = self.state.lock();
        if !state.accepting {
            return Err(SystemError::ENETDOWN);
        }
        if state.packets.len() >= Self::MAX_FRAMES
            || state.bytes.saturating_add(packet.len()) > Self::MAX_BYTES
        {
            return Err(SystemError::ENOBUFS);
        }
        state.bytes += packet.len();
        state.packets.push_back(packet);
        Ok(())
    }

    fn pop(&self) -> Option<LocalInputPacket> {
        let mut state = self.state.lock();
        let packet = state.packets.pop_front()?;
        state.bytes -= packet.len();
        Some(packet)
    }

    fn is_empty(&self) -> bool {
        self.state.lock().packets.is_empty()
    }

    /// Linearizes local-input admission with link transitions. Disabling also
    /// drops packets accepted before the DOWN publication, so a later UP can
    /// never replay stale namespace-local traffic.
    fn set_accepting(&self, accepting: bool) {
        let mut state = self.state.lock();
        state.accepting = accepting;
        if !accepting {
            state.packets.clear();
            state.bytes = 0;
        }
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) enum IfacePollScope {
    None,
    Full,
}

/// A receive-only view over the shared namespace-local input queue. Replies
/// use the real device's transmit token, so ARP/L2 and driver ownership remain
/// with the target interface while the injected frame never enters its wire
/// receive ring.
struct LocalInputDevice<'a, D: SmolDevice + ?Sized> {
    device: &'a mut D,
    queue: &'a LocalInputQueue,
}

impl<'a, D: SmolDevice + ?Sized> LocalInputDevice<'a, D> {
    fn new(device: &'a mut D, queue: &'a LocalInputQueue) -> Self {
        Self { device, queue }
    }
}

impl<D: SmolDevice + ?Sized> SmolDevice for LocalInputDevice<'_, D> {
    type RxToken<'a>
        = LocalInputRxToken
    where
        Self: 'a;
    type TxToken<'a>
        = D::TxToken<'a>
    where
        Self: 'a;

    fn receive(
        &mut self,
        timestamp: smoltcp::time::Instant,
    ) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        if self.queue.is_empty() {
            return None;
        }
        let medium = self.device.capabilities().medium;
        let tx = self.device.transmit(timestamp)?;
        let packet = self.queue.pop()?;
        let frame = packet.into_frame(medium);
        Some((LocalInputRxToken { frame }, tx))
    }

    fn transmit(&mut self, timestamp: smoltcp::time::Instant) -> Option<Self::TxToken<'_>> {
        self.device.transmit(timestamp)
    }

    fn capabilities(&self) -> DeviceCapabilities {
        self.device.capabilities()
    }
}

pub struct IfaceCommon {
    iface_id: usize,
    name: RwLock<String>,
    flags: AtomicU32,
    mtu: AtomicUsize,
    type_: InterfaceType,
    smol_iface: Mutex<smoltcp::iface::Interface>,
    /// 存smoltcp网卡的套接字集
    sockets: Mutex<smoltcp::iface::SocketSet<'static>>,
    /// 存 kernel wrap smoltcp socket 的集合
    bounds: RwLock<Arc<Vec<Arc<dyn InetSocket>>>>,
    /// 端口管理器
    port_manager: PortManager,
    /// Scheduler-owned future protocol deadline. Immediate work stays with
    /// the current poll owner and is never armed here.
    poll_deadline: PollDeadline,
    /// 网络命名空间
    net_namespace: RwLock<Weak<NetNamespace>>,
    /// 路由相关数据
    router_common_data: RouterEnableDeviceCommon,
    /// NAPI 结构体
    napi_struct: RwLock<Option<Arc<NapiStruct>>>,
    /// Namespace-local frames handed to this interface's protocol stack.
    /// This is shared by every interface implementation so weak-host local
    /// delivery never depends on a driver-specific receive queue.
    local_input_queue: LocalInputQueue,
    /// Per-address control-plane state committed together with smoltcp's
    /// address list. It carries Linux IFA_LABEL semantics and opaque ownership
    /// tokens for in-kernel actors such as DHCP.
    address_metadata: Mutex<Vec<AddressMetadata>>,
    /// Routes supplied by constructors before the interface joins a netns.
    /// Drained transactionally by netns registration; never authoritative.
    bootstrap_routes: Mutex<Vec<BootstrapRoute>>,
    static_neighbors: RwSem<Vec<StaticNeighborEntry>>,
    /// TCP close(2) 语义辅助：延迟回收 smoltcp TCP socket（Linux-like）。
    tcp_close_defer: crate::net::tcp_close_defer::TcpCloseDefer,
    /// TCP listener/backlog 语义辅助（Linux-like 丢 SYN 等）。
    tcp_listener_backlog: crate::net::tcp_listener_backlog::TcpListenerBacklog,
    ipv4_multicast_refcnt: Mutex<Vec<(smoltcp::wire::Ipv4Address, usize)>>,
    /// Serializes configured receive-mode flags with AF_PACKET references.
    receive_mode: Mutex<ReceiveModeState>,
}

impl fmt::Debug for IfaceCommon {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IfaceCommon")
            .field("iface_id", &self.iface_id)
            .field("poll_deadline", &self.poll_deadline)
            .finish()
    }
}

impl IfaceCommon {
    pub fn new(
        iface_id: usize,
        type_: InterfaceType,
        name: String,
        mtu: usize,
        flags: InterfaceFlags,
        iface: smoltcp::iface::Interface,
    ) -> Self {
        let router_common_data = RouterEnableDeviceCommon::default();
        router_common_data
            .ip_addrs
            .write()
            .extend_from_slice(iface.ip_addrs());
        let address_metadata = iface
            .ip_addrs()
            .iter()
            .map(|cidr| AddressMetadata {
                cidr: *cidr,
                label: None,
            })
            .collect();
        IfaceCommon {
            iface_id,
            name: RwLock::new(name),
            smol_iface: Mutex::new(iface),
            sockets: Mutex::new(smoltcp::iface::SocketSet::new(Vec::new())),
            bounds: RwLock::new(Arc::new(Vec::new())),
            port_manager: PortManager::default(),
            poll_deadline: PollDeadline::new(),
            net_namespace: RwLock::new(Weak::new()),
            router_common_data,
            flags: AtomicU32::new(flags.bits()),
            mtu: AtomicUsize::new(mtu),
            type_,
            napi_struct: RwLock::new(None),
            local_input_queue: LocalInputQueue::new(flags.contains(InterfaceFlags::UP)),
            address_metadata: Mutex::new(address_metadata),
            bootstrap_routes: Mutex::new(Vec::new()),
            static_neighbors: RwSem::new(Vec::new()),
            tcp_close_defer: crate::net::tcp_close_defer::TcpCloseDefer::new(),
            tcp_listener_backlog: crate::net::tcp_listener_backlog::TcpListenerBacklog::new(),
            ipv4_multicast_refcnt: Mutex::new(Vec::new()),
            receive_mode: Mutex::new(ReceiveModeState {
                configured_flags: flags.bits(),
                packet_promiscuity: 0,
                packet_allmulti: 0,
            }),
        }
    }

    /// Register an active TCP listener port on this iface.
    pub fn register_tcp_listen_port(&self, port: u16, backlog: usize) {
        self.tcp_listener_backlog
            .register_tcp_listen_port(port, backlog);
    }

    /// Unregister an active TCP listener port on this iface.
    pub fn unregister_tcp_listen_port(&self, port: u16) {
        self.tcp_listener_backlog.unregister_tcp_listen_port(port);
    }

    pub fn ipv4_multicast_join_ref(
        &self,
        group: smoltcp::wire::Ipv4Address,
    ) -> Result<(), smoltcp::iface::MulticastError> {
        let mut guard = self.ipv4_multicast_refcnt.lock();
        if let Some((_, ref mut cnt)) = guard.iter_mut().find(|(g, _)| *g == group) {
            *cnt = cnt.saturating_add(1);
            return Ok(());
        }
        self.smol_iface
            .lock()
            .join_multicast_group(smoltcp::wire::IpAddress::Ipv4(group))?;
        guard.push((group, 1));
        Ok(())
    }

    pub fn ipv4_multicast_leave_ref(&self, group: smoltcp::wire::Ipv4Address) {
        let mut guard = self.ipv4_multicast_refcnt.lock();
        let Some(pos) = guard.iter().position(|(g, _)| *g == group) else {
            return;
        };
        if guard[pos].1 > 1 {
            guard[pos].1 -= 1;
            return;
        }
        guard.swap_remove(pos);
        let _ = self
            .smol_iface
            .lock()
            .leave_multicast_group(smoltcp::wire::IpAddress::Ipv4(group));
    }

    /// 驱动收包入口使用的通用丢包策略（避免驱动理解 L4 语义）。
    #[inline]
    pub fn should_drop_rx_packet(&self, packet: &[u8]) -> bool {
        self.tcp_listener_backlog
            .should_drop_backlog_full_tcp_syn_ip(packet)
    }

    fn enqueue_local_input(&self, packet: LocalInputPacket) -> Result<(), SystemError> {
        self.local_input_queue.enqueue(packet)
    }

    pub(crate) fn has_local_input(&self) -> bool {
        !self.local_input_queue.is_empty()
    }

    pub(crate) fn poll_scope(&self) -> IfacePollScope {
        if self.flags().contains(InterfaceFlags::UP) {
            IfacePollScope::Full
        } else {
            IfacePollScope::None
        }
    }

    /// Defer removing a TCP socket from the SocketSet until it reaches Closed.
    pub fn defer_tcp_close(&self, request: crate::net::tcp_close_defer::DeferredTcpCloseRequest) {
        let now = crate::time::Instant::now().into();
        self.tcp_close_defer.defer_tcp_close(now, request);
    }

    pub fn poll<D>(&self, device: &mut D) -> bool
    where
        D: smoltcp::phy::Device + ?Sized,
    {
        match self.poll_scope() {
            IfacePollScope::None => return false,
            IfacePollScope::Full => {}
        }

        let timestamp = crate::time::Instant::now().into();
        let mut sockets = self.sockets.lock();
        let mut interface = self.smol_iface.lock();

        // 刷新 listener 缓存：必须在持有 sockets 锁的前提下进行，且不得额外分配。
        self.tcp_listener_backlog
            .refresh_listen_socket_present(&sockets);

        let (has_events, poll_again, deadline_rearm) = {
            let local_result = if self.has_local_input() {
                let mut local_device = LocalInputDevice::new(device, &self.local_input_queue);
                Some(interface.poll(timestamp, &mut local_device, &mut sockets))
            } else {
                None
            };
            let poll_result = interface.poll(timestamp, device, &mut sockets);

            // Reclaim/advance orphaned TCP sockets after smoltcp has processed ingress.
            // If this aborts an orphan, compute poll_at afterwards so the pending RST is
            // scheduled immediately instead of waiting for an unrelated future poll.
            self.tcp_close_defer.reap_closed(timestamp, &mut sockets);

            let poll_at = interface.poll_at(timestamp, &sockets);
            let (poll_again, deadline_rearm) = self.publish_poll_deadline(timestamp, poll_at);
            (
                local_result.is_some_and(|result| {
                    matches!(result, smoltcp::iface::PollResult::SocketStateChanged)
                }) || matches!(poll_result, smoltcp::iface::PollResult::SocketStateChanged),
                poll_again || self.has_local_input(),
                deadline_rearm,
            )
        };

        // Publish the deadline while the smoltcp serialization locks are held,
        // but notify the namespace only after dropping them.
        drop(interface);
        drop(sockets);
        self.notify_deadline_rearm(deadline_rearm);

        // 注意：不要在持有 bounds 读锁(且 irqsave)期间调用 socket.notify()。
        // 否则会形成典型锁顺序反转死锁：
        // - poll 路径：bounds.read_irqsave() -> socket.notify() -> socket.inner(RwLock)
        // - connect/bind/close 路径：socket.inner(RwLock) -> bounds.write()
        // 因此这里先快照一份 bound sockets，再逐个 notify。
        //
        // IMPORTANT: 对于 loopback 场景（如 gVisor BlockingLargeWrite 测试），始终需要唤醒所有
        // 等待的 socket。原因：smoltcp 在处理 ACK 后可能不返回 SocketStateChanged，但发送端的
        // can_send() 已经变为 true。如果只在 has_events 时唤醒，发送端会永远等待。
        // 唤醒后 socket 会重新检查条件，如果条件不满足会继续等待，所以不会造成忙等待。
        self.notify_all_bound_sockets();

        // TODO: remove closed sockets
        // let closed_sockets = self
        //     .closing_sockets
        //     .lock_irq_disabled()
        //     .extract_if(|closing_socket| closing_socket.is_closed())
        //     .collect::<Vec<_>>();
        // drop(closed_sockets);
        // `Iface::poll()` 的返回值不仅用于“这轮有没有状态变化”，还会被
        // `poll_iface_until_quiescent()` 当作“是否还需要立刻再 poll 一轮”的判据。
        //
        // smoltcp 会通过 `poll_at() == Now` 表示还有立即可推进的工作
        // （例如 loopback 二次往返、ACK/window update、仅 egress 前进等）。
        // 如果这里只返回 `has_events`，快路径会过早停止，剩余工作只能等下一次外部事件，
        // 在 blocking TCP 大包场景就会表现为 send/recv 偶发永久卡住。
        has_events || poll_again
    }

    /// Atomically classify and, when due, claim this interface's future
    /// protocol deadline.
    #[inline]
    pub fn classify_poll_deadline(&self, now_us: u64) -> DueResult {
        self.poll_deadline.classify_and_claim(now_us)
    }

    /// Restore a claimed deadline after a failed scheduler handoff, without
    /// overwriting a concurrent publisher.
    #[inline]
    pub fn restore_poll_deadline(&self, claimed_us: u64) -> bool {
        self.poll_deadline.restore_claimed_if_empty(claimed_us)
    }

    /// NAPI 使用的 bounded poll：最多处理 `budget` 个 ingress 包，然后推进一次 egress。
    ///
    /// 返回值语义：是否仍有 ingress backlog 需要继续 poll（即 budget 用尽且仍在处理包）。
    pub fn poll_napi<D>(&self, device: &mut D, budget: usize) -> napi::NapiPollResult
    where
        D: smoltcp::phy::Device + ?Sized,
    {
        match self.poll_scope() {
            IfacePollScope::None => return napi::NapiPollResult::idle(),
            IfacePollScope::Full => {}
        }

        let timestamp = crate::time::Instant::now().into();
        let mut sockets = self.sockets.lock();
        let mut interface = self.smol_iface.lock();

        // 刷新 listener 缓存：必须在持有 sockets 锁的前提下进行，且不得额外分配。
        self.tcp_listener_backlog
            .refresh_listen_socket_present(&sockets);

        let mut processed = 0usize;
        let mut had_packet = false;

        // Reserve at most half of the first pass for namespace-local handoff,
        // then poll the hardware/device queue. If the device has no work, use
        // the remaining budget for local input. This keeps both sources
        // progressing without reducing throughput when only one is active.
        let local_first_budget = budget.div_ceil(2);
        {
            let mut local_device = LocalInputDevice::new(device, &self.local_input_queue);
            for _ in 0..local_first_budget {
                match interface.poll_ingress_single(timestamp, &mut local_device, &mut sockets) {
                    smoltcp::iface::PollIngressSingleResult::None => break,
                    smoltcp::iface::PollIngressSingleResult::PacketProcessed
                    | smoltcp::iface::PollIngressSingleResult::SocketStateChanged => {
                        had_packet = true;
                        processed += 1;
                    }
                }
            }
        }

        let device_budget = budget - processed;
        let mut device_processed = 0usize;
        for _ in 0..device_budget {
            match interface.poll_ingress_single(timestamp, device, &mut sockets) {
                smoltcp::iface::PollIngressSingleResult::None => break,
                smoltcp::iface::PollIngressSingleResult::PacketProcessed
                | smoltcp::iface::PollIngressSingleResult::SocketStateChanged => {
                    had_packet = true;
                    processed += 1;
                    device_processed += 1;
                }
            }
        }

        let remaining = device_budget - device_processed;
        if remaining > 0 && self.has_local_input() {
            let mut local_device = LocalInputDevice::new(device, &self.local_input_queue);
            for _ in 0..remaining {
                match interface.poll_ingress_single(timestamp, &mut local_device, &mut sockets) {
                    smoltcp::iface::PollIngressSingleResult::None => break,
                    smoltcp::iface::PollIngressSingleResult::PacketProcessed
                    | smoltcp::iface::PollIngressSingleResult::SocketStateChanged => {
                        had_packet = true;
                        processed += 1;
                    }
                }
            }
        }

        // 推进发送路径（smoltcp 保证 bounded work）。
        let _ = interface.poll_egress(timestamp, device, &mut sockets);

        let poll_at = interface.poll_at(timestamp, &sockets);
        let (poll_again, deadline_rearm) = self.publish_poll_deadline(timestamp, poll_at);

        // 解锁后唤醒/通知 socket（沿用原 poll() 的 Linux-like 语义）。
        drop(interface);
        drop(sockets);
        self.notify_deadline_rearm(deadline_rearm);
        self.notify_all_bound_sockets();

        // NAPI 语义：只要“还有立即可推进的工作”，就应继续留在 poll_list。
        //
        // 除了 ingress backlog 超过 budget 之外，egress/ACK 路径也可能要求立刻再次 poll：
        // smoltcp 会通过 `poll_at() == Now`（这里被 clamp 成 `Some(timestamp)`）表达这一点。
        // 如果忽略这个条件，loopback/TCP 大流量场景可能出现：
        // - 已处理完当前 ingress batch；
        // - 但仍有 ACK / window update / 后续 egress 需要立即发送；
        // - NAPI 线程却错误睡眠，直到下一次外部事件才继续推进，
        //   导致 send done 后 recv 端偶发卡住。
        napi::NapiPollResult::new(
            processed,
            (had_packet && processed == budget) || poll_again || self.has_local_input(),
        )
    }

    /// Publish smoltcp's next scheduling decision while both smoltcp
    /// serialization locks are held.
    ///
    /// The returned boolean pair is `(poll_again, deadline_rearm)`.
    fn publish_poll_deadline(
        &self,
        now: smoltcp::time::Instant,
        poll_at: Option<smoltcp::time::Instant>,
    ) -> (bool, bool) {
        match poll_at {
            Some(instant) if instant <= now => {
                self.poll_deadline.disarm();
                (true, false)
            }
            Some(instant) => {
                let now_us = now.total_micros() as u64;
                let deadline_us = instant.total_micros() as u64;
                let rearm = self.poll_deadline.publish_future(now_us, deadline_us)
                    == PublishResult::RearmRequired;
                (false, rearm)
            }
            None => {
                self.poll_deadline.disarm();
                (false, false)
            }
        }
    }

    fn notify_deadline_rearm(&self, rearm: bool) {
        if !rearm {
            return;
        }
        if let Some(netns) = self.net_namespace() {
            netns.notify_deadline_changed();
        }
    }

    // 需要bounds储存具体的Inet Socket信息，以提供不同种类inet socket的事件分发
    pub fn bind_socket(&self, socket: Arc<dyn InetSocket>) {
        Arc::make_mut(&mut *self.bounds.write()).push(socket);
    }

    pub fn unbind_socket(&self, socket: Arc<dyn InetSocket>) {
        let mut bounds = self.bounds.write();
        let bounds = Arc::make_mut(&mut *bounds);
        if let Some(index) = bounds.iter().position(|s| Arc::ptr_eq(s, &socket)) {
            bounds.remove(index);
            // log::debug!("unbind socket success");
        }
    }

    /// Notify all bound sockets unconditionally.
    /// This is used after listener shutdown to ensure all client sockets
    /// are woken up even if the interface poll didn't detect any events.
    pub fn notify_all_bound_sockets(&self) {
        // Take one coherent snapshot before dropping the lock. Iterating by index while
        // repeatedly releasing the lock can skip sockets when a concurrent close removes
        // an earlier element and shifts the Vec to the left.
        // Use a single read-side critical section. A size pass followed by a copy pass
        // can be forced behind the stream of close-side writers between acquisitions,
        // delaying network progress long enough for poll waiters to time out.
        // Clone only the outer Arc while IRQs are disabled.  Mutations use
        // Arc::make_mut(), so this remains a coherent snapshot without an
        // O(n) allocation in the polling hot path.
        let sockets = self.bounds.read_irqsave().clone();
        for sock in sockets.iter() {
            sock.notify();
            let _woke = sock.wait_queue().wakeup(Some(ProcessState::Blocked(true)));
        }
    }

    pub fn ipv4_addr(&self) -> Option<Ipv4Addr> {
        self.smol_iface.lock().ipv4_addr()
    }

    pub fn ip_addrs(&self) -> RwSemReadGuard<'_, Vec<smoltcp::wire::IpCidr>> {
        self.router_common_data.ip_addrs.read()
    }

    pub fn prefix_len(&self) -> Option<u8> {
        self.smol_iface
            .lock()
            .ip_addrs()
            .first()
            .map(|ip_addr| ip_addr.prefix_len())
    }

    pub fn net_namespace(&self) -> Option<Arc<NetNamespace>> {
        self.net_namespace.read().upgrade()
    }

    pub fn set_net_namespace(&self, ns: Arc<NetNamespace>) {
        let mut guard = self.net_namespace.write();
        *guard = Arc::downgrade(&ns);
    }

    pub fn clear_net_namespace(&self) {
        *self.net_namespace.write() = Weak::new();
    }

    /// Runs a construction-time mutation while preventing namespace
    /// publication from starting. This is the lifecycle barrier shared by
    /// all bootstrap state owned by an interface.
    pub(crate) fn with_unpublished<T>(
        &self,
        mutation: impl FnOnce() -> Result<T, SystemError>,
    ) -> Result<T, SystemError> {
        let namespace = self.net_namespace.read();
        if namespace.upgrade().is_some() {
            return Err(SystemError::EBUSY);
        }
        mutation()
    }

    pub fn name(&self) -> String {
        self.name.read().clone()
    }

    pub fn set_name(&self, name: String) {
        *self.name.write() = name;
    }

    pub fn flags(&self) -> InterfaceFlags {
        InterfaceFlags::from_bits_truncate(self.flags.load(Ordering::Acquire))
    }

    pub fn update_configured_flags(
        &self,
        requested: InterfaceFlags,
        change_mask: InterfaceFlags,
    ) -> Result<(InterfaceFlags, InterfaceFlags), SystemError> {
        let mut state = self.receive_mode.lock();
        let old = InterfaceFlags::from_bits_truncate(state.configured_flags);
        let configured = (state.configured_flags & !change_mask.bits())
            | (requested.bits() & change_mask.bits());

        Self::total_receive_mode_count(
            state.packet_promiscuity,
            configured & InterfaceFlags::PROMISC.bits() != 0,
        )?;
        Self::total_receive_mode_count(
            state.packet_allmulti,
            configured & InterfaceFlags::ALLMULTI.bits() != 0,
        )?;

        state.configured_flags = configured;
        self.publish_effective_flags(&state);
        Ok((old, InterfaceFlags::from_bits_truncate(configured)))
    }

    pub fn link_flags_snapshot(&self) -> Result<LinkFlagsSnapshot, SystemError> {
        let state = self.receive_mode.lock();
        let configured = InterfaceFlags::from_bits_truncate(state.configured_flags);
        Ok(LinkFlagsSnapshot {
            configured,
            promiscuity: Self::total_receive_mode_count(
                state.packet_promiscuity,
                configured.contains(InterfaceFlags::PROMISC),
            )?,
            allmulti: Self::total_receive_mode_count(
                state.packet_allmulti,
                configured.contains(InterfaceFlags::ALLMULTI),
            )?,
        })
    }

    pub fn configured_flags(&self) -> InterfaceFlags {
        InterfaceFlags::from_bits_truncate(self.receive_mode.lock().configured_flags)
    }

    pub fn type_(&self) -> InterfaceType {
        self.type_
    }

    pub fn mtu(&self) -> usize {
        self.mtu.load(Ordering::Relaxed)
    }

    pub fn set_mtu(&self, mtu: usize) {
        self.mtu.store(mtu, Ordering::Relaxed);
    }

    pub(crate) fn address_metadata(&self) -> &Mutex<Vec<AddressMetadata>> {
        &self.address_metadata
    }

    /// Stages a route before publication. Holding the namespace read guard
    /// closes the race with `set_net_namespace()` during registration.
    pub(crate) fn stage_bootstrap_route(&self, route: BootstrapRoute) -> Result<(), SystemError> {
        self.with_unpublished(|| {
            let expected_oif = u32::try_from(self.iface_id).map_err(|_| SystemError::EOVERFLOW)?;
            if route.oif != expected_oif {
                return Err(SystemError::EINVAL);
            }

            let mut routes = self.bootstrap_routes.lock();
            if routes.contains(&route) {
                return Err(SystemError::EEXIST);
            }
            routes.try_reserve(1).map_err(|_| SystemError::ENOMEM)?;
            routes.push(route);
            Ok(())
        })
    }

    /// Transfers unpublished routes to netns registration without cloning.
    pub(crate) fn take_bootstrap_routes(&self) -> Vec<BootstrapRoute> {
        core::mem::take(&mut *self.bootstrap_routes.lock())
    }

    /// Restores the hand-off buffer when netns registration rolls back.
    pub(crate) fn restore_bootstrap_routes(&self, routes: Vec<BootstrapRoute>) {
        let mut staged = self.bootstrap_routes.lock();
        debug_assert!(staged.is_empty());
        *staged = routes;
    }

    pub fn static_neighbors(&self) -> RwSemReadGuard<'_, Vec<StaticNeighborEntry>> {
        self.static_neighbors.read()
    }

    pub fn set_static_neighbor(&self, entry: StaticNeighborEntry) {
        let mut neighbors = self.static_neighbors.write();
        if let Some(existing) = neighbors
            .iter_mut()
            .find(|existing| existing.ip_addr == entry.ip_addr)
        {
            *existing = entry;
        } else {
            neighbors.push(entry);
        }
    }

    pub fn remove_static_neighbor(&self, ip_addr: smoltcp::wire::IpAddress) -> bool {
        let mut neighbors = self.static_neighbors.write();
        let before = neighbors.len();
        neighbors.retain(|existing| existing.ip_addr != ip_addr);
        neighbors.len() != before
    }

    pub fn adjust_promiscuity(&self, inc: i32) -> Result<(), SystemError> {
        self.adjust_receive_mode(InterfaceFlags::PROMISC, inc)
    }

    pub fn adjust_allmulti(&self, inc: i32) -> Result<(), SystemError> {
        self.adjust_receive_mode(InterfaceFlags::ALLMULTI, inc)
    }

    fn adjust_receive_mode(&self, flag: InterfaceFlags, inc: i32) -> Result<(), SystemError> {
        let mut state = self.receive_mode.lock();
        let old = if flag == InterfaceFlags::PROMISC {
            state.packet_promiscuity
        } else if flag == InterfaceFlags::ALLMULTI {
            state.packet_allmulti
        } else {
            return Err(SystemError::EINVAL);
        };
        let new = match inc {
            1 => old.checked_add(1).ok_or(SystemError::EOVERFLOW)?,
            -1 => old.checked_sub(1).ok_or(SystemError::EINVAL)?,
            _ => return Err(SystemError::EINVAL),
        };
        let configured = state.configured_flags & flag.bits() != 0;
        Self::total_receive_mode_count(new, configured)?;

        if flag == InterfaceFlags::PROMISC {
            state.packet_promiscuity = new;
        } else {
            state.packet_allmulti = new;
        }
        self.publish_effective_flags(&state);
        Ok(())
    }

    fn total_receive_mode_count(packet: u32, configured: bool) -> Result<u32, SystemError> {
        packet
            .checked_add(u32::from(configured))
            .ok_or(SystemError::EOVERFLOW)
    }

    fn publish_effective_flags(&self, state: &ReceiveModeState) {
        let mut effective = state.configured_flags;
        if state.packet_promiscuity != 0 {
            effective |= InterfaceFlags::PROMISC.bits();
        }
        if state.packet_allmulti != 0 {
            effective |= InterfaceFlags::ALLMULTI.bits();
        }
        self.flags.store(effective, Ordering::Release);
        self.local_input_queue
            .set_accepting(effective & InterfaceFlags::UP.bits() != 0);
    }
}
