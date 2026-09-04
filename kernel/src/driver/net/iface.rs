use super::*;

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
            Err(smoltcp::iface::Ipv4PacketDispatchError::Exhausted) => {
                Err(SystemError::EAGAIN_OR_EWOULDBLOCK.into())
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

    fn set_net_namespace(&self, ns: Arc<NetNamespace>) -> Result<(), SystemError> {
        self.common().set_net_namespace(ns)
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
pub(super) fn register_netdevice(
    netns: &Arc<NetNamespace>,
    dev: Arc<dyn Iface>,
) -> Result<(), SystemError> {
    // Register driver-core/sysfs first. Until PRESENT and the namespace/FIB
    // transaction both commit, the object is provisional and unannounced.
    netdev_register_kobject(dev.clone())?;
    dev.set_net_state(NetDeivceState::__LINK_STATE_PRESENT);

    if let Err(error) = netns.add_device(dev.clone()) {
        // No add uevent has been sent, so rollback is structural and must not
        // allocate notification state before removing the provisional object.
        dev.clear_net_state(NetDeivceState::__LINK_STATE_PRESENT);
        netdev_unregister_kobject(dev);
        return Err(error);
    }

    netdev_emit_uevent(dev, "add");

    Ok(())
}

#[derive(Debug)]
pub(super) struct ReceiveModeState {
    pub(super) configured_flags: u32,
    pub(super) packet_promiscuity: u32,
    pub(super) packet_allmulti: u32,
}

#[derive(Debug, Clone, Copy)]
pub struct LinkFlagsSnapshot {
    pub configured: InterfaceFlags,
    pub promiscuity: u32,
    pub allmulti: u32,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct PreparedConfiguredFlags {
    pub(super) old: InterfaceFlags,
    pub(super) new: InterfaceFlags,
}

impl PreparedConfiguredFlags {
    pub(crate) fn old_flags(self) -> InterfaceFlags {
        self.old
    }

    pub(crate) fn new_flags(self) -> InterfaceFlags {
        self.new
    }
}
