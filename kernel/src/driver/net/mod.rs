use alloc::collections::VecDeque;
use alloc::ffi::CString;
use alloc::sync::Weak;
use alloc::{fmt, vec::Vec};
use alloc::{string::String, sync::Arc};
use core::cell::Cell;
use core::net::Ipv4Addr;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
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
use smoltcp::phy::{
    Device as SmolDevice, DeviceCapabilities, PacketMeta, RxToken, TxToken as SmolTxToken,
};
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

#[derive(Debug)]
pub enum RouteSendError {
    /// Neighbor discovery has started; retrying before this deadline cannot progress it.
    RetryAt {
        retry_at: smoltcp::time::Instant,
        probe_sent: bool,
    },
    /// A device or packet error with normal errno semantics.
    Failed(SystemError),
}

impl From<SystemError> for RouteSendError {
    fn from(error: SystemError) -> Self {
        Self::Failed(error)
    }
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

    /// Submit a frame whose allocation may be transferred to the driver.
    /// Drivers with an owned receive queue can override this to avoid copying;
    /// borrowed-only DMA drivers keep the common fallback.
    fn raw_transmit_owned(&self, frame: Vec<u8>) -> Result<(), SystemError> {
        self.raw_transmit(&frame)
    }

    /// Sends an already-routed IPv4 packet through this interface.
    ///
    /// Ethernet devices share this implementation so FIB eligibility cannot
    /// drift from driver-specific forwarding hooks. smoltcp resolves the
    /// explicit next hop (or uses a permanent rtnetlink neighbor) while the
    /// caller-owned token only prepares a frame. The driver is invoked after
    /// releasing the smoltcp lock.
    fn route_and_send(
        &self,
        next_hop: &smoltcp::wire::IpAddress,
        ip_packet: &[u8],
    ) -> Result<(), RouteSendError> {
        let smoltcp::wire::IpAddress::Ipv4(next_hop) = *next_hop else {
            return Err(SystemError::EAFNOSUPPORT.into());
        };
        let frame_capacity = (14usize + ip_packet.len()).max(42);
        let mut frame = Vec::new();
        frame
            .try_reserve_exact(frame_capacity)
            .map_err(|_| RouteSendError::Failed(SystemError::ENOMEM))?;
        let frame_prepared = Cell::new(false);
        let permanent_neighbor = self
            .common()
            .static_neighbors()
            .iter()
            .find(|entry| entry.ip_addr == smoltcp::wire::IpAddress::Ipv4(next_hop))
            .map(|entry| entry.hw_addr);

        let dispatch = {
            let mut interface = self.smol_iface().lock();
            interface.dispatch_ipv4_packet(
                crate::time::Instant::now().into(),
                PreparedFrameTxToken {
                    frame: &mut frame,
                    prepared: &frame_prepared,
                },
                next_hop,
                permanent_neighbor,
                ip_packet,
            )
        };

        let probe_sent = frame_prepared.get();
        if probe_sent {
            self.raw_transmit_owned(frame)
                .map_err(RouteSendError::Failed)?;
        }
        match dispatch {
            Ok(()) => Ok(()),
            Err(smoltcp::iface::Ipv4PacketDispatchError::NeighborPending { retry_at }) => {
                Err(RouteSendError::RetryAt {
                    retry_at,
                    probe_sent,
                })
            }
            Err(smoltcp::iface::Ipv4PacketDispatchError::NoRoute) => {
                Err(SystemError::ENETUNREACH.into())
            }
            Err(smoltcp::iface::Ipv4PacketDispatchError::Malformed)
            | Err(smoltcp::iface::Ipv4PacketDispatchError::InvalidHardwareAddress) => {
                Err(SystemError::EINVAL.into())
            }
        }
    }

    /// Send an already-routed IPv4 packet, transferring ownership to this
    /// interface's bounded output queue when immediate progress is impossible.
    /// The caller may consume the ingress packet after this returns `Ok(())`.
    fn route_and_send_or_queue(
        &self,
        next_hop: &smoltcp::wire::IpAddress,
        ip_packet: &[u8],
    ) -> Result<(), SystemError> {
        let smoltcp::wire::IpAddress::Ipv4(next_hop) = *next_hop else {
            return Err(SystemError::EAFNOSUPPORT);
        };
        let napi = self.napi_struct();
        let netns = napi.is_none().then(|| self.net_namespace()).flatten();
        if napi.is_none() && netns.is_none() {
            return Err(SystemError::ENODEV);
        }
        if let Some(retry_at) = self.common().enqueue_existing_routed_output(
            self.nic_id() as u32,
            next_hop,
            ip_packet,
        )? {
            self.common().schedule_local_output(retry_at, napi, netns);
            return Ok(());
        }
        let tx_generation = self.common().tx_completion_generation();
        let (retry_at, probe_sent) =
            match self.route_and_send(&smoltcp::wire::IpAddress::Ipv4(next_hop), ip_packet) {
                Ok(()) => return Ok(()),
                Err(RouteSendError::RetryAt {
                    retry_at,
                    probe_sent,
                }) => (retry_at, probe_sent),
                Err(RouteSendError::Failed(SystemError::ENOBUFS))
                | Err(RouteSendError::Failed(SystemError::EAGAIN_OR_EWOULDBLOCK)) => {
                    let now: smoltcp::time::Instant = crate::time::Instant::now().into();
                    let delay_us = self.common().next_local_output_tx_backoff_us();
                    let retry_at = now + smoltcp::time::Duration::from_micros(delay_us);
                    let (packet, reservation) = self.common().prepare_routed_output(
                        self.nic_id() as u32,
                        next_hop,
                        ip_packet,
                    )?;
                    reservation.requeue_backpressured(packet, retry_at);
                    let retry_at = if self.common().release_tx_backpressure_after(tx_generation) {
                        now
                    } else {
                        retry_at
                    };
                    self.common().schedule_local_output(retry_at, napi, netns);
                    return Ok(());
                }
                Err(RouteSendError::Failed(error)) => return Err(error),
            };

        self.common().enqueue_routed_output(
            self.nic_id() as u32,
            next_hop,
            ip_packet,
            retry_at,
            probe_sent,
        )?;
        self.common().schedule_local_output(retry_at, napi, netns);
        Ok(())
    }

    /// Hands a namespace-local IPv4 packet to this interface's protocol stack
    /// without emitting it on the link. The shared queue in `IfaceCommon`
    /// makes local delivery a protocol-stack capability rather than an
    /// optional device-driver feature.
    fn inject_local_ipv4_packet(
        &self,
        ingress_ifindex: u32,
        source_mac: smoltcp::wire::EthernetAddress,
        ip_packet: &[u8],
        broadcast: bool,
    ) -> Result<(), SystemError> {
        let mut owned_packet = Vec::new();
        owned_packet
            .try_reserve_exact(ip_packet.len())
            .map_err(|_| SystemError::ENOMEM)?;
        owned_packet.extend_from_slice(ip_packet);
        let packet = LocalInputPacket {
            ingress_ifindex,
            destination_mac: if broadcast {
                smoltcp::wire::EthernetAddress::BROADCAST
            } else {
                self.mac()
            },
            source_mac,
            ip_packet: owned_packet,
        };

        let napi = self.napi_struct();
        let netns = napi.is_none().then(|| self.net_namespace()).flatten();
        if napi.is_none() && netns.is_none() {
            return Err(SystemError::ENODEV);
        }
        self.common().enqueue_local_input(packet)?;
        self.common()
            .namespace_routed_stack
            .store(true, Ordering::Release);
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

struct PreparedFrameTxToken<'a> {
    frame: &'a mut Vec<u8>,
    prepared: &'a Cell<bool>,
}

impl SmolTxToken for PreparedFrameTxToken<'_> {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        debug_assert!(len <= self.frame.capacity());
        self.frame.resize(len, 0);
        let result = f(self.frame.as_mut_slice());
        self.prepared.set(true);
        result
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

#[derive(Debug, Clone, Copy)]
pub(crate) struct PreparedConfiguredFlags {
    old: InterfaceFlags,
    new: InterfaceFlags,
}

impl PreparedConfiguredFlags {
    pub(crate) fn old_flags(self) -> InterfaceFlags {
        self.old
    }

    pub(crate) fn new_flags(self) -> InterfaceFlags {
        self.new
    }
}

struct LocalInputRxToken {
    frame: Vec<u8>,
    meta: PacketMeta,
}

impl RxToken for LocalInputRxToken {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        f(&self.frame)
    }

    fn meta(&self) -> PacketMeta {
        self.meta
    }
}

#[derive(Debug)]
struct LocalInputPacket {
    ingress_ifindex: u32,
    destination_mac: smoltcp::wire::EthernetAddress,
    source_mac: smoltcp::wire::EthernetAddress,
    ip_packet: Vec<u8>,
}

impl LocalInputPacket {
    fn len(&self) -> usize {
        self.ip_packet.len()
    }

    fn into_frame(self, medium: smoltcp::phy::Medium) -> Result<Vec<u8>, SystemError> {
        if medium == smoltcp::phy::Medium::Ip {
            return Ok(self.ip_packet);
        }
        let frame_len = 14usize
            .checked_add(self.ip_packet.len())
            .ok_or(SystemError::EMSGSIZE)?;
        let mut frame = Vec::new();
        frame
            .try_reserve_exact(frame_len)
            .map_err(|_| SystemError::ENOMEM)?;
        frame.extend_from_slice(&self.destination_mac.0);
        frame.extend_from_slice(&self.source_mac.0);
        frame.extend_from_slice(&[0x08, 0x00]);
        frame.extend_from_slice(&self.ip_packet);
        Ok(frame)
    }
}

#[derive(Debug)]
struct LocalInputQueueState {
    packets: VecDeque<LocalInputPacket>,
    bytes: usize,
}

#[derive(Debug)]
struct LocalOutputPacket {
    medium: smoltcp::phy::Medium,
    meta: PacketMeta,
    disposition: LocalOutputDisposition,
    frame: Vec<u8>,
}

#[derive(Debug)]
struct BackpressuredLocalOutput {
    retry_at: smoltcp::time::Instant,
    packet: LocalOutputPacket,
}

#[derive(Debug)]
struct DeferredRouteBucket {
    oif: u32,
    next_hop: smoltcp::wire::Ipv4Address,
    retry_at: smoltcp::time::Instant,
    probes: u8,
    probe_in_flight: bool,
    probe_bytes: usize,
    resolved: bool,
    packets: VecDeque<LocalOutputPacket>,
    bytes: usize,
}

/// Immutable output policy chosen before entering the smoltcp serialization
/// locks. A queued packet is never reclassified against a later FIB snapshot.
#[derive(Debug, Clone, Copy)]
enum LocalOutputDisposition {
    NativeOwner,
    Local {
        oif: u32,
        ip_mtu: usize,
    },
    Routed {
        oif: u32,
        next_hop: smoltcp::wire::Ipv4Address,
        ip_mtu: usize,
    },
    Drop,
}

impl LocalOutputDisposition {
    const DROP_CONTEXT: u64 = 0;
    const LOCAL_CONTEXT: u64 = 1 << 63;

    fn routed_context(oif: u32, next_hop: smoltcp::wire::Ipv4Address) -> u64 {
        debug_assert_ne!(oif, 0);
        debug_assert_eq!(oif & (1 << 31), 0);
        ((oif as u64) << 32) | u32::from_be_bytes(next_hop.octets()) as u64
    }

    fn local_context(oif: u32) -> u64 {
        debug_assert_ne!(oif, 0);
        debug_assert_eq!(oif & (1 << 31), 0);
        Self::LOCAL_CONTEXT | ((oif as u64) << 32)
    }

    fn from_context(context: u64, ip_mtu: usize) -> Self {
        if context & Self::LOCAL_CONTEXT != 0 {
            return Self::Local {
                oif: ((context >> 32) as u32) & !(1 << 31),
                ip_mtu,
            };
        }
        let oif = (context >> 32) as u32;
        if oif == 0 {
            return Self::Drop;
        }
        let octets = (context as u32).to_be_bytes();
        Self::Routed {
            oif,
            next_hop: smoltcp::wire::Ipv4Address::new(octets[0], octets[1], octets[2], octets[3]),
            ip_mtu,
        }
    }
}

#[derive(Debug, Default)]
struct LocalOutputQueueState {
    packets: VecDeque<LocalOutputPacket>,
    backpressured: VecDeque<BackpressuredLocalOutput>,
    deferred_routes: Vec<DeferredRouteBucket>,
    frames: usize,
    bytes: usize,
    reserved_frames: usize,
    reserved_bytes: usize,
}

#[derive(Debug, Default)]
struct LocalOutputScratchPool {
    buffers: Vec<Vec<u8>>,
    bytes: usize,
}

#[derive(Debug)]
struct LocalInputQueue {
    state: SpinLock<LocalInputQueueState>,
    response_scratch: SpinLock<LocalOutputScratchPool>,
    output: SpinLock<LocalOutputQueueState>,
    output_draining: AtomicBool,
}

struct LocalOutputDrainGuard<'a> {
    draining: &'a AtomicBool,
    active: bool,
}

struct LocalOutputReservation<'a> {
    output: &'a SpinLock<LocalOutputQueueState>,
    bytes: usize,
    active: bool,
}

enum LocalOutputPop<'a> {
    Ready(
        LocalOutputPacket,
        LocalOutputReservation<'a>,
        Option<DeferredRouteKey>,
    ),
    DeferredUntil(smoltcp::time::Instant),
    Empty,
}

enum ExistingDeferredCommit<'a> {
    Queued(smoltcp::time::Instant),
    Missing(LocalOutputPacket, LocalOutputReservation<'a>),
    Full(LocalOutputPacket, LocalOutputReservation<'a>),
}

enum ExistingDeferredEnqueue<'a> {
    Queued(smoltcp::time::Instant),
    Missing(LocalOutputPacket, LocalOutputReservation<'a>),
    Full(LocalOutputPacket),
}

enum AdmittedRoutedOutput {
    Sent(LocalOutputPacket),
    Queued(smoltcp::time::Instant),
    Drop(LocalOutputPacket, SystemError),
}

#[derive(Clone, Copy)]
struct DeferredRouteKey {
    oif: u32,
    next_hop: smoltcp::wire::Ipv4Address,
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
    fn finish_and_has_output(mut self, output: &SpinLock<LocalOutputQueueState>) -> bool {
        let output = output.lock();
        self.draining.store(false, Ordering::Release);
        self.active = false;
        !output.packets.is_empty()
            || !output.backpressured.is_empty()
            || !output.deferred_routes.is_empty()
    }
}

impl<'a> LocalOutputReservation<'a> {
    /// Move this token's byte reservation without changing the queue-wide
    /// frame reservation. This is used when routing selects a larger MTU.
    fn try_resize(&mut self, bytes: usize) -> bool {
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

    fn commit(
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
        });
    }

    fn requeue_backpressured(
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
        let queued = BackpressuredLocalOutput { retry_at, packet };
        if output
            .backpressured
            .back()
            .is_none_or(|tail| tail.retry_at <= retry_at)
        {
            output.backpressured.push_back(queued);
        } else {
            let index = output
                .backpressured
                .iter()
                .position(|queued| queued.retry_at > retry_at)
                .expect("a later backpressure deadline exists");
            output.backpressured.insert(index, queued);
        }
        self.active = false;
    }

    fn requeue_deferred(
        self,
        packet: LocalOutputPacket,
        retry_at: smoltcp::time::Instant,
        probe_sent: bool,
    ) -> Result<(), LocalOutputPacket> {
        self.commit_deferred_packet(packet, retry_at, probe_sent, true)
    }

    fn commit_packet(mut self, packet: LocalOutputPacket) {
        debug_assert_eq!(packet.frame.capacity(), self.bytes);
        let mut output = self.output.lock();
        output.reserved_frames -= 1;
        output.reserved_bytes -= self.bytes;
        output.frames += 1;
        output.bytes += self.bytes;
        output.packets.push_back(packet);
        self.active = false;
    }

    fn commit_deferred_packet(
        mut self,
        packet: LocalOutputPacket,
        retry_at: smoltcp::time::Instant,
        probe_sent: bool,
        advance_existing_probe: bool,
    ) -> Result<(), LocalOutputPacket> {
        let LocalOutputDisposition::Routed { oif, next_hop, .. } = packet.disposition else {
            return Err(packet);
        };
        let mut output = self.output.lock();
        let bucket_index = output
            .deferred_routes
            .iter()
            .position(|bucket| bucket.oif == oif && bucket.next_hop == next_hop);
        if let Some(index) = bucket_index {
            let bucket = &mut output.deferred_routes[index];
            if bucket.packets.len() + usize::from(bucket.probe_in_flight)
                >= LocalInputQueue::MAX_DEFERRED_FRAMES_PER_NEIGHBOR
                || bucket
                    .bytes
                    .saturating_add(bucket.probe_bytes)
                    .saturating_add(self.bytes)
                    > LocalInputQueue::MAX_DEFERRED_BYTES_PER_NEIGHBOR
                || bucket.packets.try_reserve(1).is_err()
            {
                drop(output);
                return Err(packet);
            }
            bucket.retry_at = if probe_sent || advance_existing_probe {
                retry_at
            } else {
                bucket.retry_at.min(retry_at)
            };
            if probe_sent {
                bucket.probes = bucket.probes.saturating_add(1);
            }
            bucket.bytes += self.bytes;
            bucket.packets.push_back(packet);
        } else {
            if output.deferred_routes.try_reserve(1).is_err() {
                drop(output);
                return Err(packet);
            }
            let mut packets = VecDeque::new();
            if packets.try_reserve(1).is_err() {
                drop(output);
                return Err(packet);
            }
            packets.push_back(packet);
            output.deferred_routes.push(DeferredRouteBucket {
                oif,
                next_hop,
                retry_at,
                probes: u8::from(probe_sent),
                probe_in_flight: false,
                probe_bytes: 0,
                resolved: false,
                packets,
                bytes: self.bytes,
            });
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
    fn commit_existing_deferred(mut self, packet: LocalOutputPacket) -> ExistingDeferredCommit<'a> {
        let LocalOutputDisposition::Routed { oif, next_hop, .. } = packet.disposition else {
            return ExistingDeferredCommit::Missing(packet, self);
        };
        debug_assert_eq!(packet.frame.capacity(), self.bytes);
        let mut output = self.output.lock();
        let Some(index) = output
            .deferred_routes
            .iter()
            .position(|bucket| bucket.oif == oif && bucket.next_hop == next_hop)
        else {
            drop(output);
            return ExistingDeferredCommit::Missing(packet, self);
        };
        let bucket = &mut output.deferred_routes[index];
        if bucket.packets.len() + usize::from(bucket.probe_in_flight)
            >= LocalInputQueue::MAX_DEFERRED_FRAMES_PER_NEIGHBOR
            || bucket
                .bytes
                .saturating_add(bucket.probe_bytes)
                .saturating_add(self.bytes)
                > LocalInputQueue::MAX_DEFERRED_BYTES_PER_NEIGHBOR
            || bucket.packets.try_reserve(1).is_err()
        {
            drop(output);
            return ExistingDeferredCommit::Full(packet, self);
        }
        let retry_at = bucket.retry_at;
        bucket.bytes += self.bytes;
        bucket.packets.push_back(packet);
        output.reserved_frames -= 1;
        output.reserved_bytes -= self.bytes;
        output.frames += 1;
        output.bytes += self.bytes;
        self.active = false;
        ExistingDeferredCommit::Queued(retry_at)
    }

    fn finish_deferred_probe(
        mut self,
        packet: LocalOutputPacket,
        key: DeferredRouteKey,
        retry_at: smoltcp::time::Instant,
        probe_sent: bool,
    ) -> Result<bool, LocalOutputPacket> {
        debug_assert_eq!(packet.frame.capacity(), self.bytes);
        let mut output = self.output.lock();
        let Some(index) = output.deferred_routes.iter().position(|bucket| {
            bucket.oif == key.oif && bucket.next_hop == key.next_hop && bucket.probe_in_flight
        }) else {
            drop(output);
            return Err(packet);
        };

        let resolved = output.deferred_routes[index].resolved;
        {
            let bucket = &mut output.deferred_routes[index];
            debug_assert_eq!(bucket.probe_bytes, self.bytes);
            bucket.probe_in_flight = false;
            bucket.probe_bytes = 0;
            bucket.bytes += self.bytes;
            bucket.packets.push_front(packet);
            if !resolved {
                bucket.retry_at = retry_at;
                if probe_sent {
                    bucket.probes = bucket.probes.saturating_add(1);
                }
            }
        }

        output.reserved_frames -= 1;
        output.reserved_bytes -= self.bytes;
        output.frames += 1;
        output.bytes += self.bytes;
        if resolved {
            let mut bucket = output.deferred_routes.swap_remove(index);
            output.packets.append(&mut bucket.packets);
        }
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
    const MAX_FRAMES: usize = 1024;
    const MAX_BYTES: usize = 4 * 1024 * 1024;
    const MAX_DEFERRED_FRAMES_PER_NEIGHBOR: usize = 64;
    const MAX_DEFERRED_BYTES_PER_NEIGHBOR: usize = 256 * 1024;
    const MAX_NEIGHBOR_PROBES: u8 = 3;
    const MAX_SCRATCH_FRAMES: usize = 64;
    const MAX_SCRATCH_BYTES: usize = 256 * 1024;

    fn new() -> Self {
        Self {
            state: SpinLock::new(LocalInputQueueState {
                packets: VecDeque::new(),
                bytes: 0,
            }),
            response_scratch: SpinLock::new(LocalOutputScratchPool::default()),
            output: SpinLock::new(LocalOutputQueueState::default()),
            output_draining: AtomicBool::new(false),
        }
    }

    fn enqueue(&self, packet: LocalInputPacket) -> Result<(), SystemError> {
        let mut state = self.state.lock();
        if state.packets.len() >= Self::MAX_FRAMES
            || state.bytes.saturating_add(packet.len()) > Self::MAX_BYTES
        {
            return Err(SystemError::ENOBUFS);
        }
        state
            .packets
            .try_reserve(1)
            .map_err(|_| SystemError::ENOMEM)?;
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

    fn reserve_output(&self) -> Option<LocalOutputReservation<'_>> {
        let mut output = self.output.lock();
        if output.frames.saturating_add(output.reserved_frames) >= Self::MAX_FRAMES {
            return None;
        }
        let additional = output
            .frames
            .saturating_add(output.reserved_frames)
            .saturating_add(1)
            .saturating_sub(output.packets.len());
        output.packets.try_reserve(additional).ok()?;
        let additional_backpressured = output
            .frames
            .saturating_add(output.reserved_frames)
            .saturating_add(1)
            .saturating_sub(output.backpressured.len());
        output
            .backpressured
            .try_reserve(additional_backpressured)
            .ok()?;
        output.reserved_frames += 1;
        Some(LocalOutputReservation {
            output: &self.output,
            bytes: 0,
            active: true,
        })
    }

    fn pop_ready_output(
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
                .pop_front()
                .expect("front was observed above");
            output.packets.push_back(queued.packet);
        }
        let due_index = (prefer_deferred || output.packets.is_empty()).then(|| {
            output
                .deferred_routes
                .iter()
                .position(|bucket| !bucket.probe_in_flight && bucket.retry_at <= now)
        });
        let due_index = due_index.flatten();
        // Give one due neighbor bucket priority per drain round. The rest of
        // the round preserves FIFO service for ready output.
        let take_deferred = due_index.is_some() && (prefer_deferred || output.packets.is_empty());
        let (packet, probe_key) = if take_deferred {
            let index = due_index.expect("take_deferred requires a due bucket");
            if output.deferred_routes[index].resolved
                || output.deferred_routes[index].probes >= Self::MAX_NEIGHBOR_PROBES
            {
                let mut bucket = output.deferred_routes.swap_remove(index);
                if !bucket.resolved {
                    for packet in bucket.packets.iter_mut() {
                        packet.disposition = LocalOutputDisposition::Drop;
                    }
                }
                let packet = bucket.packets.pop_front();
                output.packets.append(&mut bucket.packets);
                (packet, None)
            } else {
                let bucket = &mut output.deferred_routes[index];
                let packet = bucket
                    .packets
                    .pop_front()
                    .expect("a deferred route bucket is never empty");
                let bytes = packet.frame.capacity();
                bucket.bytes -= bytes;
                bucket.probe_in_flight = true;
                bucket.probe_bytes = bytes;
                (
                    Some(packet),
                    Some(DeferredRouteKey {
                        oif: bucket.oif,
                        next_hop: bucket.next_hop,
                    }),
                )
            }
        } else if let Some(packet) = output.packets.pop_front() {
            (Some(packet), None)
        } else {
            (None, None)
        };
        if let Some(packet) = packet {
            let bytes = packet.frame.capacity();
            output.frames -= 1;
            output.bytes -= bytes;
            output.reserved_frames += 1;
            output.reserved_bytes += bytes;
            return LocalOutputPop::Ready(
                packet,
                LocalOutputReservation {
                    output: &self.output,
                    bytes,
                    active: true,
                },
                probe_key,
            );
        }
        match output
            .deferred_routes
            .iter()
            .filter(|bucket| !bucket.probe_in_flight)
            .map(|bucket| bucket.retry_at)
            .chain(output.backpressured.front().map(|queued| queued.retry_at))
            .min()
        {
            Some(retry_at) => LocalOutputPop::DeferredUntil(retry_at),
            None => LocalOutputPop::Empty,
        }
    }

    fn has_output(&self) -> bool {
        let output = self.output.lock();
        !output.packets.is_empty()
            || !output.backpressured.is_empty()
            || !output.deferred_routes.is_empty()
    }

    fn release_backpressured_outputs(&self) -> bool {
        let mut output = self.output.lock();
        if output.backpressured.is_empty() {
            return false;
        }
        while let Some(queued) = output.backpressured.pop_front() {
            output.packets.push_back(queued.packet);
        }
        true
    }

    fn has_deferred_output(&self) -> bool {
        !self.output.lock().deferred_routes.is_empty()
    }

    fn release_resolved_outputs(
        &self,
        mut is_resolved: impl FnMut(smoltcp::wire::Ipv4Address) -> bool,
    ) {
        let mut output = self.output.lock();
        let mut index = 0;
        while index < output.deferred_routes.len() {
            if is_resolved(output.deferred_routes[index].next_hop) {
                if output.deferred_routes[index].probe_in_flight {
                    output.deferred_routes[index].resolved = true;
                    index += 1;
                } else {
                    let mut bucket = output.deferred_routes.swap_remove(index);
                    output.packets.append(&mut bucket.packets);
                }
            } else {
                index += 1;
            }
        }
    }

    fn release_neighbor(&self, oif: u32, next_hop: smoltcp::wire::Ipv4Address) -> bool {
        let mut output = self.output.lock();
        let Some(index) = output
            .deferred_routes
            .iter()
            .position(|bucket| bucket.oif == oif && bucket.next_hop == next_hop)
        else {
            return false;
        };
        if output.deferred_routes[index].probe_in_flight {
            output.deferred_routes[index].resolved = true;
        } else {
            let mut bucket = output.deferred_routes.swap_remove(index);
            output.packets.append(&mut bucket.packets);
        }
        true
    }

    fn complete_deferred_probe_success(&self, key: DeferredRouteKey) {
        let mut output = self.output.lock();
        let Some(index) = output.deferred_routes.iter().position(|bucket| {
            bucket.oif == key.oif && bucket.next_hop == key.next_hop && bucket.probe_in_flight
        }) else {
            return;
        };
        let mut bucket = output.deferred_routes.swap_remove(index);
        output.packets.append(&mut bucket.packets);
    }

    /// Fail only the in-flight representative. Other packets in this bucket
    /// may still be valid and remain eligible for independent processing.
    fn complete_deferred_packet_failure(&self, key: DeferredRouteKey) {
        let mut output = self.output.lock();
        let Some(index) = output.deferred_routes.iter().position(|bucket| {
            bucket.oif == key.oif && bucket.next_hop == key.next_hop && bucket.probe_in_flight
        }) else {
            return;
        };
        let bucket = &mut output.deferred_routes[index];
        bucket.probe_in_flight = false;
        bucket.probe_bytes = 0;
        if bucket.packets.is_empty() {
            output.deferred_routes.swap_remove(index);
        }
    }

    fn clear_routed_if_idle(
        &self,
        routed: &AtomicBool,
        bound_socket_count: &AtomicUsize,
        deferred_close_pending: bool,
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
            && !deferred_close_pending
            && !routed_fragments_pending
        {
            routed.store(false, Ordering::Release);
        }
    }

    fn try_begin_output_drain(&self) -> Option<LocalOutputDrainGuard<'_>> {
        self.output_draining
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .ok()?;
        Some(LocalOutputDrainGuard {
            draining: &self.output_draining,
            active: true,
        })
    }

    fn finish_output_drain(&self, guard: LocalOutputDrainGuard<'_>) -> bool {
        guard.finish_and_has_output(&self.output)
    }

    fn recycle_output(&self, frame: Vec<u8>) {
        Self::recycle_scratch(&self.response_scratch, frame);
    }

    fn recycle_scratch(pool: &SpinLock<LocalOutputScratchPool>, mut frame: Vec<u8>) {
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

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) enum IfacePollScope {
    None,
    LocalOnly,
    Full,
}

/// A namespace-local view over the target interface's transport stack.
/// Ingress retains the physical ifindex. Output is staged until the smoltcp
/// locks are released: IPv4 may then select another device through the
/// namespace FIB, while native same-interface and non-IPv4 traffic keeps the
/// underlying device path.
struct LocalInputDevice<'a, D: SmolDevice + ?Sized> {
    device: &'a mut D,
    common: &'a IfaceCommon,
    route_policy: &'a crate::net::route::OutputRouteGuard<'a>,
    owner_ifindex: u32,
    owner_is_up: bool,
}

/// Delegates receive to the physical device while routing every response and
/// standalone IPv4 transmission through the same deferred output FIFO as
/// namespace-local input.
struct RoutedTxDevice<'a, D: SmolDevice + ?Sized> {
    device: &'a mut D,
    queue: &'a LocalInputQueue,
    route_policy: &'a crate::net::route::OutputRouteGuard<'a>,
    owner_ifindex: u32,
    owner_is_up: bool,
}

/// An owned response buffer temporarily checked out from an interface-local
/// pool. Pool locking is limited to checkout/return; smoltcp and driver
/// callbacks never run while holding it.
struct LocalInputScratch<'a> {
    buffer: Option<Vec<u8>>,
    pool: &'a SpinLock<LocalOutputScratchPool>,
}

impl<'a> LocalInputScratch<'a> {
    fn checkout(pool: &'a SpinLock<LocalOutputScratchPool>, capacity: usize) -> Option<Self> {
        let mut buffer = {
            let mut pooled = pool.lock();
            let buffer = pooled.buffers.pop().unwrap_or_default();
            pooled.bytes -= buffer.capacity();
            buffer
        };
        buffer.clear();
        if buffer.try_reserve_exact(capacity).is_err() {
            LocalInputQueue::recycle_scratch(pool, buffer);
            return None;
        }
        Some(Self {
            buffer: Some(buffer),
            pool,
        })
    }

    fn resize(&mut self, len: usize) -> &mut [u8] {
        let buffer = self
            .buffer
            .as_mut()
            .expect("checked-out scratch always owns its buffer");
        debug_assert!(len <= buffer.capacity());
        buffer.resize(len, 0);
        buffer.as_mut_slice()
    }

    fn try_ensure_capacity(
        &mut self,
        capacity: usize,
        reservation: &mut LocalOutputReservation<'_>,
    ) -> bool {
        let buffer = self
            .buffer
            .as_mut()
            .expect("checked-out scratch always owns its buffer");
        if buffer.capacity() >= capacity {
            return true;
        }
        debug_assert!(buffer.is_empty());
        let mut replacement = Vec::new();
        if replacement.try_reserve_exact(capacity).is_err()
            || !reservation.try_resize(replacement.capacity())
        {
            return false;
        }
        core::mem::swap(buffer, &mut replacement);
        LocalInputQueue::recycle_scratch(self.pool, replacement);
        true
    }

    fn capacity(&self) -> usize {
        self.buffer.as_ref().map_or(0, Vec::capacity)
    }

    fn take(&mut self) -> Option<Vec<u8>> {
        self.buffer.take()
    }
}

impl Drop for LocalInputScratch<'_> {
    fn drop(&mut self) {
        let Some(buffer) = self.buffer.take() else {
            return;
        };
        LocalInputQueue::recycle_scratch(self.pool, buffer);
    }
}

/// A response token paired with namespace-local ingress.
///
/// A local-stack response is completed in memory and then sent through the
/// namespace FIB. This keeps transport progress independent of the address
/// owner's physical TX queue and lets output select its own interface.
struct LocalInputTxToken<'a> {
    medium: smoltcp::phy::Medium,
    meta: PacketMeta,
    disposition: LocalOutputDisposition,
    route_policy: &'a crate::net::route::OutputRouteGuard<'a>,
    owner_ifindex: u32,
    owner_is_up: bool,
    owner_ip_mtu: usize,
    reservation: LocalOutputReservation<'a>,
    scratch: LocalInputScratch<'a>,
}

impl SmolTxToken for LocalInputTxToken<'_> {
    fn egress_override(
        &mut self,
        version: smoltcp::wire::IpVersion,
        destination: smoltcp::wire::IpAddress,
        meta: PacketMeta,
    ) -> Option<smoltcp::phy::TxEgressOverride> {
        if version != smoltcp::wire::IpVersion::Ipv4 {
            return None;
        }
        let constrained_oif = (meta.id != 0).then_some(meta.id);
        if let Some(route) = self.route_policy.lookup(destination, constrained_oif) {
            if route.kind == crate::net::route::RTN_LOCAL {
                let route_ip_mtu = core::cmp::min(route.ip_mtu, u16::MAX as usize);
                let ip_mtu = if self
                    .scratch
                    .try_ensure_capacity(route_ip_mtu, &mut self.reservation)
                {
                    route_ip_mtu
                } else {
                    core::cmp::min(route_ip_mtu, self.owner_ip_mtu)
                };
                self.medium = smoltcp::phy::Medium::Ip;
                self.disposition = LocalOutputDisposition::Local {
                    oif: route.oif,
                    ip_mtu,
                };
                return Some(smoltcp::phy::TxEgressOverride {
                    medium: smoltcp::phy::Medium::Ip,
                    ip_mtu,
                    context: LocalOutputDisposition::local_context(route.oif),
                });
            }
            if route.oif == self.owner_ifindex && self.owner_is_up {
                return None;
            }
            let smoltcp::wire::IpAddress::Ipv4(next_hop) = route.next_hop else {
                self.medium = smoltcp::phy::Medium::Ip;
                self.disposition = LocalOutputDisposition::Drop;
                return Some(smoltcp::phy::TxEgressOverride {
                    medium: smoltcp::phy::Medium::Ip,
                    ip_mtu: self.owner_ip_mtu,
                    context: LocalOutputDisposition::DROP_CONTEXT,
                });
            };
            self.medium = smoltcp::phy::Medium::Ip;
            // The address owner and selected egress may have different MTUs.
            // Grow only the token that actually needs the larger route MTU;
            // on allocation pressure, a smaller legal fragment/drop boundary
            // is preferable to constructing beyond the scratch capacity.
            // IPv4's total-length field is the hard protocol ceiling even if
            // userspace configured a larger logical device MTU.
            let route_ip_mtu = core::cmp::min(route.ip_mtu, u16::MAX as usize);
            let ip_mtu = if self
                .scratch
                .try_ensure_capacity(route_ip_mtu, &mut self.reservation)
            {
                route_ip_mtu
            } else {
                core::cmp::min(route_ip_mtu, self.owner_ip_mtu)
            };
            self.disposition = LocalOutputDisposition::Routed {
                oif: route.oif,
                next_hop,
                ip_mtu,
            };
            return Some(smoltcp::phy::TxEgressOverride {
                medium: smoltcp::phy::Medium::Ip,
                ip_mtu,
                context: LocalOutputDisposition::routed_context(route.oif, next_hop),
            });
        }
        self.medium = smoltcp::phy::Medium::Ip;
        self.disposition = LocalOutputDisposition::Drop;
        Some(smoltcp::phy::TxEgressOverride {
            medium: smoltcp::phy::Medium::Ip,
            ip_mtu: self.owner_ip_mtu,
            context: LocalOutputDisposition::DROP_CONTEXT,
        })
    }

    fn apply_egress_override(&mut self, egress: smoltcp::phy::TxEgressOverride) -> bool {
        if !self
            .scratch
            .try_ensure_capacity(egress.ip_mtu, &mut self.reservation)
        {
            self.disposition = LocalOutputDisposition::Drop;
            return false;
        }
        self.medium = egress.medium;
        self.disposition = LocalOutputDisposition::from_context(egress.context, egress.ip_mtu);
        true
    }

    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let Self {
            medium,
            meta,
            disposition,
            route_policy: _,
            owner_ifindex: _,
            owner_is_up: _,
            owner_ip_mtu: _,
            reservation,
            mut scratch,
        } = self;
        let result = f(scratch.resize(len));
        reservation.commit(medium, meta, disposition, &mut scratch);
        result
    }

    fn set_meta(&mut self, meta: PacketMeta) {
        self.meta = meta;
    }
}

impl<'a, D: SmolDevice + ?Sized> LocalInputDevice<'a, D> {
    fn new(
        device: &'a mut D,
        common: &'a IfaceCommon,
        route_policy: &'a crate::net::route::OutputRouteGuard<'a>,
        owner_ifindex: u32,
        owner_is_up: bool,
    ) -> Self {
        Self {
            device,
            common,
            route_policy,
            owner_ifindex,
            owner_is_up,
        }
    }

    fn tx_token(&self) -> Option<LocalInputTxToken<'a>> {
        local_tx_token(
            &self.common.local_input_queue,
            self.route_policy,
            self.owner_ifindex,
            self.owner_is_up,
            self.device.capabilities(),
        )
    }
}

fn local_tx_token<'a>(
    queue: &'a LocalInputQueue,
    route_policy: &'a crate::net::route::OutputRouteGuard<'a>,
    owner_ifindex: u32,
    owner_is_up: bool,
    capabilities: DeviceCapabilities,
) -> Option<LocalInputTxToken<'a>> {
    let mut reservation = queue.reserve_output()?;
    let scratch =
        LocalInputScratch::checkout(&queue.response_scratch, capabilities.max_transmission_unit)?;
    if !reservation.try_resize(scratch.capacity()) {
        return None;
    }
    Some(LocalInputTxToken {
        medium: capabilities.medium,
        meta: PacketMeta::default(),
        disposition: LocalOutputDisposition::NativeOwner,
        route_policy,
        owner_ifindex,
        owner_is_up,
        owner_ip_mtu: capabilities.ip_mtu(),
        reservation,
        scratch,
    })
}

impl<D: SmolDevice + ?Sized> SmolDevice for LocalInputDevice<'_, D> {
    type RxToken<'a>
        = LocalInputRxToken
    where
        Self: 'a;
    type TxToken<'a>
        = LocalInputTxToken<'a>
    where
        Self: 'a;

    fn receive(
        &mut self,
        _timestamp: smoltcp::time::Instant,
    ) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        // Reserve response capacity before consuming ingress. If the bounded
        // output queue is full, smoltcp observes device backpressure and the
        // input remains queued for a later poll.
        let tx_token = self.tx_token()?;
        let packet = self.common.local_input_queue.pop()?;
        // Namespace-local delivery is an ingress path in its own right.
        // Apply the same pre-stack policy as a driver receive queue so a
        // routed local packet cannot bypass listener/backlog semantics. Stop
        // this ingress round after one policy drop to keep NAPI work bounded;
        // the non-empty local queue schedules the next round.
        if self.common.should_drop_rx_packet(&packet.ip_packet) {
            return None;
        }
        let ingress_ifindex = packet.ingress_ifindex;
        let frame = packet.into_frame(self.device.capabilities().medium).ok()?;
        let mut meta = PacketMeta::default();
        meta.id = ingress_ifindex;
        Some((LocalInputRxToken { frame, meta }, tx_token))
    }

    fn transmit(&mut self, _timestamp: smoltcp::time::Instant) -> Option<Self::TxToken<'_>> {
        self.tx_token()
    }

    fn capabilities(&self) -> DeviceCapabilities {
        self.device.capabilities()
    }
}

impl<D: SmolDevice + ?Sized> SmolDevice for RoutedTxDevice<'_, D> {
    type RxToken<'a>
        = D::RxToken<'a>
    where
        Self: 'a;
    type TxToken<'a>
        = LocalInputTxToken<'a>
    where
        Self: 'a;

    fn receive(
        &mut self,
        timestamp: smoltcp::time::Instant,
    ) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        let capabilities = self.device.capabilities();
        let tx_token = local_tx_token(
            self.queue,
            self.route_policy,
            self.owner_ifindex,
            self.owner_is_up,
            capabilities,
        )?;
        let (rx_token, physical_tx_token) = self.device.receive(timestamp)?;
        drop(physical_tx_token);
        Some((rx_token, tx_token))
    }

    fn transmit(&mut self, _timestamp: smoltcp::time::Instant) -> Option<Self::TxToken<'_>> {
        local_tx_token(
            self.queue,
            self.route_policy,
            self.owner_ifindex,
            self.owner_is_up,
            self.device.capabilities(),
        )
    }

    fn capabilities(&self) -> DeviceCapabilities {
        self.device.capabilities()
    }
}

enum LocalOutputTransmitResult {
    Sent(LocalOutputPacket),
    RetrySoon(LocalOutputPacket),
    RetryAt {
        packet: LocalOutputPacket,
        retry_at: smoltcp::time::Instant,
        probe_sent: bool,
    },
    Drop(LocalOutputPacket, SystemError),
}

fn output_error(packet: LocalOutputPacket, error: SystemError) -> LocalOutputTransmitResult {
    match error {
        SystemError::ENOBUFS | SystemError::EAGAIN_OR_EWOULDBLOCK => {
            LocalOutputTransmitResult::RetrySoon(packet)
        }
        _ => LocalOutputTransmitResult::Drop(packet, error),
    }
}

fn transmit_routed_stack_output(
    iface: &dyn Iface,
    packet: LocalOutputPacket,
) -> LocalOutputTransmitResult {
    let LocalOutputDisposition::Routed {
        next_hop, ip_mtu, ..
    } = packet.disposition
    else {
        return LocalOutputTransmitResult::Drop(packet, SystemError::EINVAL);
    };
    if packet.medium != smoltcp::phy::Medium::Ip
        || packet.frame.len() > ip_mtu
        || packet.frame.first().map(|byte| byte >> 4) != Some(4)
    {
        return LocalOutputTransmitResult::Drop(packet, SystemError::EINVAL);
    }
    if !iface.flags().contains(InterfaceFlags::UP) || packet.frame.len() > iface.mtu() {
        let error = if !iface.flags().contains(InterfaceFlags::UP) {
            SystemError::ENETDOWN
        } else {
            SystemError::EMSGSIZE
        };
        return LocalOutputTransmitResult::Drop(packet, error);
    }
    match iface.route_and_send(&smoltcp::wire::IpAddress::Ipv4(next_hop), &packet.frame) {
        Ok(()) => LocalOutputTransmitResult::Sent(packet),
        Err(RouteSendError::RetryAt {
            retry_at,
            probe_sent,
        }) => LocalOutputTransmitResult::RetryAt {
            packet,
            retry_at,
            probe_sent,
        },
        Err(RouteSendError::Failed(error)) => output_error(packet, error),
    }
}

/// Transmit a routed packet after the actual egress has reserved queue
/// capacity. Any retry is committed to that egress before the caller releases
/// its source reservation, so TX-completion wakeups always target the owner of
/// the backpressured resource.
fn transmit_admitted_routed_output(
    iface: &dyn Iface,
    packet: LocalOutputPacket,
    reservation: LocalOutputReservation<'_>,
) -> AdmittedRoutedOutput {
    let LocalOutputDisposition::Routed { oif, next_hop, .. } = packet.disposition else {
        drop(reservation);
        return AdmittedRoutedOutput::Drop(packet, SystemError::EINVAL);
    };
    let tx_generation = iface.common().tx_completion_generation();
    match transmit_routed_stack_output(iface, packet) {
        LocalOutputTransmitResult::Sent(packet) => {
            iface.common().reset_local_output_tx_backoff();
            iface
                .common()
                .local_input_queue
                .release_neighbor(oif, next_hop);
            drop(reservation);
            AdmittedRoutedOutput::Sent(packet)
        }
        LocalOutputTransmitResult::RetrySoon(packet) => {
            let delay_us = iface.common().next_local_output_tx_backoff_us();
            let now: smoltcp::time::Instant = crate::time::Instant::now().into();
            let retry_at = now + smoltcp::time::Duration::from_micros(delay_us);
            reservation.requeue_backpressured(packet, retry_at);
            let retry_at = if iface.common().release_tx_backpressure_after(tx_generation) {
                now
            } else {
                retry_at
            };
            AdmittedRoutedOutput::Queued(retry_at)
        }
        LocalOutputTransmitResult::RetryAt {
            packet,
            retry_at,
            probe_sent,
        } => {
            iface.common().reset_local_output_tx_backoff();
            match reservation.requeue_deferred(packet, retry_at, probe_sent) {
                Ok(()) => AdmittedRoutedOutput::Queued(retry_at),
                Err(packet) => AdmittedRoutedOutput::Drop(packet, SystemError::ENOBUFS),
            }
        }
        LocalOutputTransmitResult::Drop(packet, error) => {
            iface.common().reset_local_output_tx_backoff();
            drop(reservation);
            AdmittedRoutedOutput::Drop(packet, error)
        }
    }
}

fn transmit_local_stack_output<D>(
    netns: &Arc<NetNamespace>,
    owner_is_up: bool,
    device: &mut D,
    packet: LocalOutputPacket,
) -> LocalOutputTransmitResult
where
    D: SmolDevice + ?Sized,
{
    match packet.disposition {
        LocalOutputDisposition::NativeOwner => {
            transmit_native_output_if_up(device, packet, owner_is_up)
        }
        LocalOutputDisposition::Drop => {
            LocalOutputTransmitResult::Drop(packet, SystemError::ENETUNREACH)
        }
        LocalOutputDisposition::Local { oif, ip_mtu } => {
            if packet.medium != smoltcp::phy::Medium::Ip
                || packet.frame.len() > ip_mtu
                || packet.frame.first().map(|byte| byte >> 4) != Some(4)
            {
                return LocalOutputTransmitResult::Drop(packet, SystemError::EINVAL);
            }
            let Some(iface) = netns.device_list().get(&(oif as usize)).cloned() else {
                return LocalOutputTransmitResult::Drop(packet, SystemError::ENODEV);
            };
            if packet.frame.len() > iface.mtu() {
                return LocalOutputTransmitResult::Drop(packet, SystemError::EMSGSIZE);
            }
            match iface.inject_local_ipv4_packet(oif, iface.mac(), &packet.frame, false) {
                Ok(()) => LocalOutputTransmitResult::Sent(packet),
                // This is receive-backlog congestion, not physical TX
                // backpressure. Linux may drop locally delivered packets when
                // the receive backlog is full; a TX completion cannot make
                // this target input queue writable.
                Err(error) => LocalOutputTransmitResult::Drop(packet, error),
            }
        }
        LocalOutputDisposition::Routed { oif, .. } => {
            let Some(iface) = netns.device_list().get(&(oif as usize)).cloned() else {
                return LocalOutputTransmitResult::Drop(packet, SystemError::ENODEV);
            };
            transmit_routed_stack_output(iface.as_ref(), packet)
        }
    }
}

fn transmit_native_output_if_up<D>(
    device: &mut D,
    packet: LocalOutputPacket,
    owner_is_up: bool,
) -> LocalOutputTransmitResult
where
    D: SmolDevice + ?Sized,
{
    if !owner_is_up {
        return LocalOutputTransmitResult::Drop(packet, SystemError::ENETDOWN);
    }
    transmit_native_output(device, packet)
}

fn transmit_native_output<D>(device: &mut D, packet: LocalOutputPacket) -> LocalOutputTransmitResult
where
    D: SmolDevice + ?Sized,
{
    let Some(mut token) = device.transmit(crate::time::Instant::now().into()) else {
        return LocalOutputTransmitResult::RetrySoon(packet);
    };
    token.set_meta(packet.meta);
    token.consume(packet.frame.len(), |buffer| {
        buffer.copy_from_slice(&packet.frame);
    });
    LocalOutputTransmitResult::Sent(packet)
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
    /// Lock-free lifecycle summary for DOWN-owner protocol progress. The
    /// vector remains authoritative; this count only decides poll eligibility.
    bound_socket_count: AtomicUsize,
    /// The stack has accepted namespace-local ingress and must keep applying
    /// authoritative output routing until its socket/deferred work quiesces.
    namespace_routed_stack: AtomicBool,
    /// 端口管理器
    port_manager: PortManager,
    /// Scheduler-owned future protocol deadline. Immediate work stays with
    /// the current poll owner and is never armed here.
    poll_deadline: PollDeadline,
    /// Bounded fallback delay for local-output device backpressure. Individual
    /// packets retain their not-before deadline across unrelated poll wakes.
    local_output_tx_backoff_us: AtomicU64,
    /// Monotonic handshake between TX completion and backpressure enqueue.
    /// A producer that observes a change after enqueue promotes the packet
    /// itself; otherwise the later completion notification does so.
    tx_completion_generation: AtomicU64,
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

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum LocalOutputDrainResult {
    Quiescent,
    BudgetExhausted,
    Backpressured,
    Contended,
}

impl LocalOutputDrainResult {
    fn needs_immediate_poll(self) -> bool {
        matches!(self, Self::BudgetExhausted)
    }
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
    const LOCAL_OUTPUT_POLL_BUDGET: usize = 64;
    const LOCAL_OUTPUT_TX_BACKOFF_MIN_US: u64 = 1_000;
    const LOCAL_OUTPUT_TX_BACKOFF_MAX_US: u64 = 100_000;

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
            bound_socket_count: AtomicUsize::new(0),
            namespace_routed_stack: AtomicBool::new(false),
            port_manager: PortManager::default(),
            poll_deadline: PollDeadline::new(),
            local_output_tx_backoff_us: AtomicU64::new(Self::LOCAL_OUTPUT_TX_BACKOFF_MIN_US),
            tx_completion_generation: AtomicU64::new(0),
            net_namespace: RwLock::new(Weak::new()),
            router_common_data,
            flags: AtomicU32::new(flags.bits()),
            mtu: AtomicUsize::new(mtu),
            type_,
            napi_struct: RwLock::new(None),
            local_input_queue: LocalInputQueue::new(),
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

    fn enqueue_routed_output(
        &self,
        oif: u32,
        next_hop: smoltcp::wire::Ipv4Address,
        ip_packet: &[u8],
        retry_at: smoltcp::time::Instant,
        probe_sent: bool,
    ) -> Result<(), SystemError> {
        let (packet, reservation) = self.prepare_routed_output(oif, next_hop, ip_packet)?;
        if let Err(packet) = reservation.commit_deferred_packet(packet, retry_at, probe_sent, false)
        {
            self.local_input_queue.recycle_output(packet.frame);
            return Err(SystemError::ENOBUFS);
        }
        Ok(())
    }

    fn prepare_routed_output(
        &self,
        oif: u32,
        next_hop: smoltcp::wire::Ipv4Address,
        ip_packet: &[u8],
    ) -> Result<(LocalOutputPacket, LocalOutputReservation<'_>), SystemError> {
        if ip_packet.len() > self.mtu.load(Ordering::Acquire) {
            return Err(SystemError::EMSGSIZE);
        }
        let mut reservation = self
            .local_input_queue
            .reserve_output()
            .ok_or(SystemError::ENOBUFS)?;
        let mut scratch =
            LocalInputScratch::checkout(&self.local_input_queue.response_scratch, ip_packet.len())
                .ok_or(SystemError::ENOMEM)?;
        if !reservation.try_resize(scratch.capacity()) {
            return Err(SystemError::ENOBUFS);
        }
        scratch.resize(ip_packet.len()).copy_from_slice(ip_packet);
        let frame = scratch
            .take()
            .expect("a deferred routed packet owns its scratch buffer");
        Ok((
            LocalOutputPacket {
                medium: smoltcp::phy::Medium::Ip,
                meta: PacketMeta::default(),
                disposition: LocalOutputDisposition::Routed {
                    oif,
                    next_hop,
                    ip_mtu: self.mtu.load(Ordering::Acquire),
                },
                frame,
            },
            reservation,
        ))
    }

    /// Join a pending neighbor atomically if it still exists. A bucket that
    /// resolves between the optimistic presence check and commit is reported
    /// as missing; callers then use the immediate transmit path and never
    /// recreate a bucket with a stale deadline.
    fn enqueue_existing_routed_output(
        &self,
        oif: u32,
        next_hop: smoltcp::wire::Ipv4Address,
        ip_packet: &[u8],
    ) -> Result<Option<smoltcp::time::Instant>, SystemError> {
        let pending = self
            .local_input_queue
            .output
            .lock()
            .deferred_routes
            .iter()
            .any(|bucket| bucket.oif == oif && bucket.next_hop == next_hop);
        if !pending {
            return Ok(None);
        }
        let (packet, reservation) = self.prepare_routed_output(oif, next_hop, ip_packet)?;
        match reservation.commit_existing_deferred(packet) {
            ExistingDeferredCommit::Queued(retry_at) => Ok(Some(retry_at)),
            ExistingDeferredCommit::Missing(packet, reservation) => {
                drop(reservation);
                self.local_input_queue.recycle_output(packet.frame);
                Ok(None)
            }
            ExistingDeferredCommit::Full(packet, reservation) => {
                drop(reservation);
                self.local_input_queue.recycle_output(packet.frame);
                Err(SystemError::ENOBUFS)
            }
        }
    }

    fn enqueue_existing_deferred_output(
        &self,
        packet: LocalOutputPacket,
    ) -> ExistingDeferredEnqueue<'_> {
        let Some(mut reservation) = self.local_input_queue.reserve_output() else {
            return ExistingDeferredEnqueue::Full(packet);
        };
        if !reservation.try_resize(packet.frame.capacity()) {
            return ExistingDeferredEnqueue::Full(packet);
        }
        match reservation.commit_existing_deferred(packet) {
            ExistingDeferredCommit::Queued(retry_at) => ExistingDeferredEnqueue::Queued(retry_at),
            ExistingDeferredCommit::Missing(packet, reservation) => {
                ExistingDeferredEnqueue::Missing(packet, reservation)
            }
            ExistingDeferredCommit::Full(packet, reservation) => {
                drop(reservation);
                ExistingDeferredEnqueue::Full(packet)
            }
        }
    }

    fn schedule_local_output(
        &self,
        retry_at: smoltcp::time::Instant,
        napi: Option<Arc<NapiStruct>>,
        netns: Option<Arc<NetNamespace>>,
    ) {
        self.namespace_routed_stack.store(true, Ordering::Release);
        self.defer_local_output_retry_at(retry_at);
        if let Some(napi) = napi {
            napi::napi_schedule(napi);
        } else if let Some(netns) = netns {
            netns.wakeup_poll_thread();
        }
    }

    fn schedule_registered_local_output(&self, retry_at: smoltcp::time::Instant) {
        let napi = self.napi_struct.read().clone();
        let netns = napi
            .is_none()
            .then(|| self.net_namespace.read().upgrade())
            .flatten();
        self.schedule_local_output(retry_at, napi, netns);
    }

    /// Make device-backpressured output runnable after the driver reports TX
    /// completion. The per-packet deadlines remain a fallback for drivers that
    /// cannot provide a precise completion notification.
    pub(crate) fn release_tx_backpressure(&self) -> bool {
        self.tx_completion_generation
            .fetch_add(1, Ordering::Release);
        let released = self.local_input_queue.release_backpressured_outputs();
        if released {
            self.reset_local_output_tx_backoff();
        }
        released
    }

    fn tx_completion_generation(&self) -> u64 {
        self.tx_completion_generation.load(Ordering::Acquire)
    }

    /// Close the completion-before-enqueue race without coupling queue locks
    /// to the driver. If the generation is unchanged, a later completion must
    /// observe the queued packet; if it changed, this side performs the
    /// release that the earlier notification could not see.
    fn release_tx_backpressure_after(&self, observed_generation: u64) -> bool {
        if self.tx_completion_generation() == observed_generation {
            return false;
        }
        let released = self.local_input_queue.release_backpressured_outputs();
        if released {
            self.reset_local_output_tx_backoff();
        }
        released
    }

    pub(crate) fn notify_tx_available(&self) {
        if !self.release_tx_backpressure() {
            return;
        }
        self.schedule_registered_local_output(crate::time::Instant::now().into());
    }

    pub(crate) fn has_local_input(&self) -> bool {
        !self.local_input_queue.is_empty()
    }

    fn release_resolved_routed_outputs(
        &self,
        interface: &mut smoltcp::iface::Interface,
        timestamp: smoltcp::time::Instant,
    ) {
        if !self.local_input_queue.has_deferred_output() {
            return;
        }
        let static_neighbors = self.static_neighbors.read();
        self.local_input_queue.release_resolved_outputs(|next_hop| {
            static_neighbors
                .iter()
                .any(|entry| entry.ip_addr == smoltcp::wire::IpAddress::Ipv4(next_hop))
                || interface
                    .is_neighbor_resolved(timestamp, smoltcp::wire::IpAddress::Ipv4(next_hop))
        });
    }

    fn has_local_work(&self) -> bool {
        self.has_local_input() || self.local_input_queue.has_output()
    }

    fn needs_namespace_routing(&self) -> bool {
        self.has_local_work()
            || self.namespace_routed_stack.load(Ordering::Acquire)
            || self.tcp_close_defer.has_pending()
    }

    fn clear_namespace_routing_if_idle(&self) {
        // Reacquire the protocol lock after output draining so the fragment
        // observation and latch clear cannot race another poller's interval
        // between consuming local ingress and publishing routed output.
        let interface = self.smol_iface.lock();
        let routed_fragments_pending = interface.has_pending_egress_override();
        self.local_input_queue.clear_routed_if_idle(
            &self.namespace_routed_stack,
            &self.bound_socket_count,
            self.tcp_close_defer.has_pending(),
            routed_fragments_pending,
        );
    }

    pub(crate) fn poll_scope(&self) -> IfacePollScope {
        if self.flags().contains(InterfaceFlags::UP) {
            IfacePollScope::Full
        } else if self.needs_namespace_routing()
            || self.bound_socket_count.load(Ordering::Acquire) != 0
        {
            IfacePollScope::LocalOnly
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
        let scope = self.poll_scope();
        match scope {
            IfacePollScope::None => return false,
            IfacePollScope::LocalOnly | IfacePollScope::Full => {}
        }

        let netns = self.net_namespace();
        let needs_routed_poll =
            self.needs_namespace_routing() || scope == IfacePollScope::LocalOnly;
        let router = if needs_routed_poll {
            netns.as_ref().map(|netns| netns.router())
        } else {
            None
        };
        let route_policy = router.as_ref().and_then(|router| {
            netns
                .as_ref()
                .map(|netns| crate::net::route::lock_output_routes(router, netns.device_list()))
        });
        let routed_this_round = route_policy.is_some();
        let owner_is_up = scope == IfacePollScope::Full;

        let timestamp = crate::time::Instant::now().into();
        let mut sockets = self.sockets.lock();
        let mut interface = self.smol_iface.lock();

        // 刷新 listener 缓存：必须在持有 sockets 锁的前提下进行，且不得额外分配。
        self.tcp_listener_backlog
            .refresh_listen_socket_present(&sockets);

        let (has_events, poll_again, deadline_rearm) = {
            let local_result = if routed_this_round
                && (self.has_local_input() || scope == IfacePollScope::LocalOnly)
            {
                let mut local_device = LocalInputDevice::new(
                    device,
                    self,
                    route_policy.as_ref().unwrap(),
                    self.iface_id as u32,
                    owner_is_up,
                );
                Some(interface.poll(timestamp, &mut local_device, &mut sockets))
            } else {
                None
            };
            let poll_result = if scope == IfacePollScope::Full {
                if routed_this_round {
                    let mut routed_device = RoutedTxDevice {
                        device,
                        queue: &self.local_input_queue,
                        route_policy: route_policy.as_ref().unwrap(),
                        owner_ifindex: self.iface_id as u32,
                        owner_is_up,
                    };
                    Some(interface.poll(timestamp, &mut routed_device, &mut sockets))
                } else {
                    Some(interface.poll(timestamp, device, &mut sockets))
                }
            } else {
                None
            };

            // Reclaim/advance orphaned TCP sockets after smoltcp has processed ingress.
            // If this aborts an orphan, compute poll_at afterwards so the pending RST is
            // scheduled immediately instead of waiting for an unrelated future poll.
            self.tcp_close_defer.reap_closed(timestamp, &mut sockets);

            self.release_resolved_routed_outputs(&mut interface, timestamp);

            let poll_at = interface.poll_at(timestamp, &sockets);
            let (poll_again, deadline_rearm) = self.publish_poll_deadline(timestamp, poll_at);
            (
                local_result.is_some_and(|result| {
                    matches!(result, smoltcp::iface::PollResult::SocketStateChanged)
                }) || poll_result.is_some_and(|result| {
                    matches!(result, smoltcp::iface::PollResult::SocketStateChanged)
                }),
                poll_again || self.has_local_input(),
                deadline_rearm,
            )
        };

        // Publish the deadline while the smoltcp serialization locks are held,
        // but notify the namespace only after dropping them.
        drop(interface);
        drop(sockets);
        drop(route_policy);
        drop(router);
        let output_drain = self.drain_local_outputs(device, Self::LOCAL_OUTPUT_POLL_BUDGET);
        self.clear_namespace_routing_if_idle();
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
        has_events || poll_again || output_drain.needs_immediate_poll()
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
        let scope = self.poll_scope();
        match scope {
            IfacePollScope::None => return napi::NapiPollResult::idle(),
            IfacePollScope::LocalOnly | IfacePollScope::Full => {}
        }

        let netns = self.net_namespace();
        let needs_routed_poll =
            self.needs_namespace_routing() || scope == IfacePollScope::LocalOnly;
        let router = if needs_routed_poll {
            netns.as_ref().map(|netns| netns.router())
        } else {
            None
        };
        let route_policy = router.as_ref().and_then(|router| {
            netns
                .as_ref()
                .map(|netns| crate::net::route::lock_output_routes(router, netns.device_list()))
        });
        let routed_this_round = route_policy.is_some();
        let owner_is_up = scope == IfacePollScope::Full;

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
        if routed_this_round {
            let mut local_device = LocalInputDevice::new(
                device,
                self,
                route_policy.as_ref().unwrap(),
                self.iface_id as u32,
                owner_is_up,
            );
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

        let device_budget = if scope == IfacePollScope::Full {
            budget - processed
        } else {
            0
        };
        let mut device_processed = 0usize;
        if routed_this_round {
            let mut routed_device = RoutedTxDevice {
                device,
                queue: &self.local_input_queue,
                route_policy: route_policy.as_ref().unwrap(),
                owner_ifindex: self.iface_id as u32,
                owner_is_up,
            };
            for _ in 0..device_budget {
                match interface.poll_ingress_single(timestamp, &mut routed_device, &mut sockets) {
                    smoltcp::iface::PollIngressSingleResult::None => break,
                    smoltcp::iface::PollIngressSingleResult::PacketProcessed
                    | smoltcp::iface::PollIngressSingleResult::SocketStateChanged => {
                        had_packet = true;
                        processed += 1;
                        device_processed += 1;
                    }
                }
            }
        } else {
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
        }

        let remaining = device_budget - device_processed;
        if routed_this_round && remaining > 0 && self.has_local_input() {
            let mut local_device = LocalInputDevice::new(
                device,
                self,
                route_policy.as_ref().unwrap(),
                self.iface_id as u32,
                owner_is_up,
            );
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
        if routed_this_round {
            let mut local_device = LocalInputDevice::new(
                device,
                self,
                route_policy.as_ref().unwrap(),
                self.iface_id as u32,
                owner_is_up,
            );
            let _ = interface.poll_egress(timestamp, &mut local_device, &mut sockets);
        } else {
            let _ = interface.poll_egress(timestamp, device, &mut sockets);
        }

        self.release_resolved_routed_outputs(&mut interface, timestamp);

        let poll_at = interface.poll_at(timestamp, &sockets);
        let (poll_again, deadline_rearm) = self.publish_poll_deadline(timestamp, poll_at);

        // 解锁后唤醒/通知 socket（沿用原 poll() 的 Linux-like 语义）。
        drop(interface);
        drop(sockets);
        drop(route_policy);
        drop(router);
        let output_drain = self.drain_local_outputs(device, budget);
        self.clear_namespace_routing_if_idle();
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
            (had_packet && processed == budget)
                || poll_again
                || self.has_local_input()
                || output_drain.needs_immediate_poll(),
        )
    }

    fn drain_local_outputs<D>(&self, device: &mut D, budget: usize) -> LocalOutputDrainResult
    where
        D: SmolDevice + ?Sized,
    {
        let Some(netns) = self.net_namespace() else {
            return LocalOutputDrainResult::Quiescent;
        };
        let Some(drain_guard) = self.local_input_queue.try_begin_output_drain() else {
            return if self.local_input_queue.has_output() {
                LocalOutputDrainResult::BudgetExhausted
            } else {
                LocalOutputDrainResult::Contended
            };
        };
        let mut prefer_deferred = true;
        for _ in 0..budget {
            let (mut output, mut in_flight, deferred_probe) = match self
                .local_input_queue
                .pop_ready_output(crate::time::Instant::now().into(), prefer_deferred)
            {
                LocalOutputPop::Ready(output, in_flight, deferred_probe) => {
                    (output, in_flight, deferred_probe)
                }
                LocalOutputPop::DeferredUntil(retry_at) => {
                    drop(drain_guard);
                    self.defer_local_output_retry_at(retry_at);
                    return LocalOutputDrainResult::Backpressured;
                }
                LocalOutputPop::Empty => {
                    return if self.local_input_queue.finish_output_drain(drain_guard) {
                        LocalOutputDrainResult::BudgetExhausted
                    } else {
                        LocalOutputDrainResult::Quiescent
                    };
                }
            };
            prefer_deferred = false;

            // Admission belongs to the actual egress interface. Atomically
            // join an existing neighbor bucket before attempting transmit so
            // neither same-owner nor cross-owner output can bypass it.
            if deferred_probe.is_none() {
                if let LocalOutputDisposition::Routed { oif, .. } = output.disposition {
                    if oif == self.iface_id as u32 {
                        match in_flight.commit_existing_deferred(output) {
                            ExistingDeferredCommit::Queued(retry_at) => {
                                self.defer_local_output_retry_at(retry_at);
                                continue;
                            }
                            ExistingDeferredCommit::Missing(packet, reservation) => {
                                output = packet;
                                in_flight = reservation;
                            }
                            ExistingDeferredCommit::Full(packet, reservation) => {
                                log::debug!(
                                    "dropping deferred output on {}: per-neighbor queue full",
                                    self.name()
                                );
                                drop(reservation);
                                self.local_input_queue.recycle_output(packet.frame);
                                continue;
                            }
                        }
                    } else if let Some(egress) = netns.device_list().get(&(oif as usize)).cloned() {
                        match egress.common().enqueue_existing_deferred_output(output) {
                            ExistingDeferredEnqueue::Queued(retry_at) => {
                                drop(in_flight);
                                egress.common().schedule_registered_local_output(retry_at);
                                continue;
                            }
                            ExistingDeferredEnqueue::Missing(packet, egress_reservation) => {
                                match transmit_admitted_routed_output(
                                    egress.as_ref(),
                                    packet,
                                    egress_reservation,
                                ) {
                                    AdmittedRoutedOutput::Sent(packet) => {
                                        drop(in_flight);
                                        self.local_input_queue.recycle_output(packet.frame);
                                    }
                                    AdmittedRoutedOutput::Queued(retry_at) => {
                                        drop(in_flight);
                                        egress.common().schedule_registered_local_output(retry_at);
                                    }
                                    AdmittedRoutedOutput::Drop(packet, error) => {
                                        log::debug!(
                                            "dropping routed output on {}: {:?}",
                                            egress.name(),
                                            error
                                        );
                                        drop(in_flight);
                                        self.local_input_queue.recycle_output(packet.frame);
                                    }
                                }
                                continue;
                            }
                            ExistingDeferredEnqueue::Full(packet) => {
                                log::debug!(
                                    "dropping deferred output on {}: egress queue full",
                                    egress.name()
                                );
                                drop(in_flight);
                                self.local_input_queue.recycle_output(packet.frame);
                                continue;
                            }
                        }
                    }
                }
            }
            let tx_generation = self.tx_completion_generation();
            match transmit_local_stack_output(
                &netns,
                self.flags().contains(InterfaceFlags::UP),
                device,
                output,
            ) {
                LocalOutputTransmitResult::Sent(output) => {
                    self.reset_local_output_tx_backoff();
                    if let Some(key) = deferred_probe {
                        self.local_input_queue.complete_deferred_probe_success(key);
                    } else if let LocalOutputDisposition::Routed { oif, next_hop, .. } =
                        output.disposition
                    {
                        self.local_input_queue.release_neighbor(oif, next_hop);
                    }
                    drop(in_flight);
                    self.local_input_queue.recycle_output(output.frame);
                }
                LocalOutputTransmitResult::Drop(output, error) => {
                    self.reset_local_output_tx_backoff();
                    log::debug!("dropping deferred output on {}: {:?}", self.name(), error);
                    if let Some(key) = deferred_probe {
                        self.local_input_queue.complete_deferred_packet_failure(key);
                    }
                    drop(in_flight);
                    self.local_input_queue.recycle_output(output.frame);
                }
                LocalOutputTransmitResult::RetrySoon(output) => {
                    let delay_us = self.next_local_output_tx_backoff_us();
                    let now: smoltcp::time::Instant = crate::time::Instant::now().into();
                    let retry_at = now + smoltcp::time::Duration::from_micros(delay_us);
                    if let Some(key) = deferred_probe {
                        // A failed physical submission waits on TX capacity,
                        // not on neighbor resolution. Return the representative
                        // to the typed TX-backpressure queue while leaving the
                        // remaining neighbor bucket eligible for another probe.
                        self.local_input_queue.complete_deferred_packet_failure(key);
                    }
                    in_flight.requeue_backpressured(output, retry_at);
                    if !self.release_tx_backpressure_after(tx_generation) {
                        self.defer_local_output_retry_at(retry_at);
                    }
                    continue;
                }
                LocalOutputTransmitResult::RetryAt {
                    packet,
                    retry_at,
                    probe_sent,
                } => {
                    self.reset_local_output_tx_backoff();
                    let LocalOutputDisposition::Routed { oif, .. } = packet.disposition else {
                        unreachable!("only routed output performs neighbor discovery");
                    };
                    if let Some(key) = deferred_probe {
                        debug_assert_eq!(oif, self.iface_id as u32);
                        match in_flight.finish_deferred_probe(packet, key, retry_at, probe_sent) {
                            Ok(resolved) => {
                                if !resolved {
                                    self.defer_local_output_retry_at(retry_at);
                                }
                            }
                            Err(packet) => {
                                self.local_input_queue.complete_deferred_packet_failure(key);
                                self.local_input_queue.recycle_output(packet.frame);
                            }
                        }
                    } else if oif == self.iface_id as u32 {
                        match in_flight.requeue_deferred(packet, retry_at, probe_sent) {
                            Ok(()) => self.defer_local_output_retry_at(retry_at),
                            Err(packet) => {
                                log::debug!(
                                    "dropping deferred output on {}: per-neighbor queue full",
                                    self.name()
                                );
                                self.local_input_queue.recycle_output(packet.frame);
                            }
                        }
                    } else {
                        // Cross-egress packets are admitted and transmitted in
                        // the branch above, while a deferred probe is always
                        // owned by this interface's neighbor bucket.
                        debug_assert!(deferred_probe.is_some());
                        drop(in_flight);
                        self.local_input_queue.recycle_output(packet.frame);
                    }
                    continue;
                }
            }
        }
        if self.local_input_queue.finish_output_drain(drain_guard) {
            LocalOutputDrainResult::BudgetExhausted
        } else {
            LocalOutputDrainResult::Quiescent
        }
    }

    fn next_local_output_tx_backoff_us(&self) -> u64 {
        self.local_output_tx_backoff_us
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                Some(
                    current
                        .saturating_mul(2)
                        .min(Self::LOCAL_OUTPUT_TX_BACKOFF_MAX_US),
                )
            })
            .unwrap_or(Self::LOCAL_OUTPUT_TX_BACKOFF_MAX_US)
    }

    fn reset_local_output_tx_backoff(&self) {
        self.local_output_tx_backoff_us
            .store(Self::LOCAL_OUTPUT_TX_BACKOFF_MIN_US, Ordering::Release);
    }

    fn defer_local_output_retry_at(&self, retry_at: smoltcp::time::Instant) {
        let now_us = crate::time::Instant::now().total_micros().max(0) as u64;
        let retry_us = (retry_at.total_micros().max(0) as u64).max(now_us.saturating_add(1));
        self.publish_local_output_retry(now_us, retry_us);
    }

    fn publish_local_output_retry(&self, now_us: u64, retry_us: u64) {
        let rearm = self.poll_deadline.publish_earlier_future(now_us, retry_us)
            == PublishResult::RearmRequired;
        self.notify_deadline_rearm(rearm);
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
        let mut bounds = self.bounds.write();
        let bounds = Arc::make_mut(&mut *bounds);
        bounds.push(socket);
        self.bound_socket_count
            .store(bounds.len(), Ordering::Release);
    }

    pub fn unbind_socket(&self, socket: Arc<dyn InetSocket>) {
        let mut bounds = self.bounds.write();
        let bounds = Arc::make_mut(&mut *bounds);
        if let Some(index) = bounds.iter().position(|s| Arc::ptr_eq(s, &socket)) {
            bounds.remove(index);
            self.bound_socket_count
                .store(bounds.len(), Ordering::Release);
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

    pub(crate) fn prepare_configured_flags(
        &self,
        requested: InterfaceFlags,
        change_mask: InterfaceFlags,
    ) -> Result<PreparedConfiguredFlags, SystemError> {
        let state = self.receive_mode.lock();
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

        Ok(PreparedConfiguredFlags {
            old,
            new: InterfaceFlags::from_bits_truncate(configured),
        })
    }

    /// Publishes a flag update whose fallible validation completed while RTNL
    /// was held. RTNL serializes configured-flag writers; packet-socket
    /// receive-mode references remain independent and are folded into the
    /// effective flags under the same lock.
    pub(crate) fn publish_configured_flags(&self, prepared: PreparedConfiguredFlags) {
        let mut state = self.receive_mode.lock();
        debug_assert_eq!(
            InterfaceFlags::from_bits_truncate(state.configured_flags),
            prepared.old
        );
        state.configured_flags = prepared.new.bits();
        self.publish_effective_flags(&state);
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
        let ip_addr = entry.ip_addr;
        let mut neighbors = self.static_neighbors.write();
        if let Some(existing) = neighbors
            .iter_mut()
            .find(|existing| existing.ip_addr == entry.ip_addr)
        {
            *existing = entry;
        } else {
            neighbors.push(entry);
        }
        drop(neighbors);
        let smoltcp::wire::IpAddress::Ipv4(next_hop) = ip_addr else {
            return;
        };
        if self
            .local_input_queue
            .release_neighbor(self.iface_id as u32, next_hop)
        {
            self.schedule_registered_local_output(crate::time::Instant::now().into());
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
    }
}
