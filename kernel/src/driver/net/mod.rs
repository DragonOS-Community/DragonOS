use alloc::sync::Weak;
use alloc::{fmt, vec::Vec};
use alloc::{string::String, sync::Arc};
use core::net::Ipv4Addr;
use sysfs::netdev_register_kobject;

use crate::driver::net::napi::NapiStruct;
use crate::driver::net::types::{InterfaceFlags, InterfaceType};
use crate::libs::rwsem::RwSemReadGuard;
use crate::net::routing::RouterEnableDeviceCommon;
use crate::net::socket::packet::PacketSocket;
use crate::process::namespace::net_namespace::NetNamespace;
use crate::{
    libs::{mutex::Mutex, rwlock::RwLock},
    net::socket::inet::{common::PortManager, InetSocket},
    process::ProcessState,
};
use smoltcp;
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

#[allow(dead_code)]
pub trait Iface: crate::driver::base::device::Device {
    /// # `common`
    /// 获取网卡的公共信息
    fn common(&self) -> &IfaceCommon;

    /// # `mac`
    /// 获取网卡的MAC地址
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
    /// ## 返回值语义（对齐 NAPI）
    /// - `true`：还有工作没做完（例如 ingress backlog 超过 budget），应继续留在 poll_list
    /// - `false`：本次已处理完，可 complete
    ///
    /// 默认实现退化为一次普通 poll（兼容旧驱动）；具体网卡应覆盖实现以保证 bounded work。
    #[inline]
    fn poll_napi(&self, _budget: usize) -> bool {
        self.poll()
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

    /// # `update_ip_addrs`
    /// 用于更新接口的 IP 地址
    /// ## 参数
    /// - `ip_addrs` ：一个包含 `smoltcp::wire::IpCidr` 的切片，表示要设置的 IP 地址和子网掩码
    /// ## 返回值
    /// - 如果 `ip_addrs` 的长度不为 1，返回 `Err(SystemError::EINVAL)`，表示输入参数无效
    fn update_ip_addrs(&self, ip_addrs: &[smoltcp::wire::IpCidr]) -> Result<(), SystemError> {
        self.common().update_ip_addrs(ip_addrs)
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

    fn operstate(&self) -> Operstate;

    fn set_operstate(&self, state: Operstate);

    fn net_namespace(&self) -> Option<Arc<NetNamespace>> {
        self.common().net_namespace()
    }

    fn set_net_namespace(&self, ns: Arc<NetNamespace>) {
        self.common().set_net_namespace(ns);
    }

    fn flags(&self) -> InterfaceFlags {
        self.common().flags()
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

/// 将网络设备注册到sysfs中
/// 参考：https://code.dragonos.org.cn/xref/linux-2.6.39/net/core/dev.c?fi=register_netdev#5373
fn register_netdevice(dev: Arc<dyn Iface>) -> Result<(), SystemError> {
    // 在sysfs中注册设备
    netdev_register_kobject(dev.clone())?;

    // 标识网络设备在系统中存在
    dev.set_net_state(NetDeivceState::__LINK_STATE_PRESENT);

    return Ok(());
}

pub struct IfaceCommon {
    iface_id: usize,
    flags: InterfaceFlags,
    type_: InterfaceType,
    smol_iface: Mutex<smoltcp::iface::Interface>,
    /// 存smoltcp网卡的套接字集
    sockets: Mutex<smoltcp::iface::SocketSet<'static>>,
    /// 存 kernel wrap smoltcp socket 的集合
    bounds: RwLock<Vec<Arc<dyn InetSocket>>>,
    /// 端口管理器
    port_manager: PortManager,
    /// 下次需要推进协议栈的时间点（单位：微秒时间戳，0 表示无定时事件）
    poll_at_us: core::sync::atomic::AtomicU64,
    /// 网络命名空间
    net_namespace: RwLock<Weak<NetNamespace>>,
    /// 路由相关数据
    router_common_data: RouterEnableDeviceCommon,
    /// NAPI 结构体
    napi_struct: RwLock<Option<Arc<NapiStruct>>>,
    /// Packet sockets registered to receive raw frames
    packet_sockets: RwLock<Vec<Weak<PacketSocket>>>,
    /// TCP close(2) 语义辅助：延迟回收 smoltcp TCP socket（Linux-like）。
    tcp_close_defer: crate::net::tcp_close_defer::TcpCloseDefer,
    /// TCP listener/backlog 语义辅助（Linux-like 丢 SYN 等）。
    tcp_listener_backlog: crate::net::tcp_listener_backlog::TcpListenerBacklog,
}

impl fmt::Debug for IfaceCommon {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IfaceCommon")
            .field("iface_id", &self.iface_id)
            .field("poll_at_us", &self.poll_at_us)
            .finish()
    }
}

impl IfaceCommon {
    pub fn new(
        iface_id: usize,
        type_: InterfaceType,
        flags: InterfaceFlags,
        iface: smoltcp::iface::Interface,
    ) -> Self {
        let router_common_data = RouterEnableDeviceCommon::default();
        router_common_data
            .ip_addrs
            .write()
            .extend_from_slice(iface.ip_addrs());
        IfaceCommon {
            iface_id,
            smol_iface: Mutex::new(iface),
            sockets: Mutex::new(smoltcp::iface::SocketSet::new(Vec::new())),
            bounds: RwLock::new(Vec::new()),
            port_manager: PortManager::default(),
            poll_at_us: core::sync::atomic::AtomicU64::new(0),
            net_namespace: RwLock::new(Weak::new()),
            router_common_data,
            flags,
            type_,
            napi_struct: RwLock::new(None),
            packet_sockets: RwLock::new(Vec::new()),
            tcp_close_defer: crate::net::tcp_close_defer::TcpCloseDefer::new(),
            tcp_listener_backlog: crate::net::tcp_listener_backlog::TcpListenerBacklog::new(),
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

    /// 驱动收包入口使用的通用丢包策略（避免驱动理解 L4 语义）。
    #[inline]
    pub fn should_drop_rx_packet(&self, packet: &[u8]) -> bool {
        self.tcp_listener_backlog
            .should_drop_backlog_full_tcp_syn_ip(packet)
    }

    /// Defer removing a TCP socket from the SocketSet until it reaches Closed.
    pub fn defer_tcp_close(
        &self,
        handle: smoltcp::iface::SocketHandle,
        local_port: u16,
        sock: alloc::sync::Weak<dyn crate::net::socket::inet::InetSocket>,
    ) {
        self.tcp_close_defer
            .defer_tcp_close(handle, local_port, sock);
    }

    pub fn poll<D>(&self, device: &mut D) -> bool
    where
        D: smoltcp::phy::Device + ?Sized,
    {
        let timestamp = crate::time::Instant::now().into();
        let mut sockets = self.sockets.lock();
        let mut interface = self.smol_iface.lock();

        // 刷新 listener 缓存：必须在持有 sockets 锁的前提下进行，且不得额外分配。
        self.tcp_listener_backlog
            .refresh_listen_socket_present(&sockets);

        let (has_events, poll_at) = {
            let poll_result = interface.poll(timestamp, device, &mut sockets);

            (
                matches!(poll_result, smoltcp::iface::PollResult::SocketStateChanged),
                {
                    // `poll_at()` may legally return an instant that is <= `timestamp`
                    // (e.g. "poll immediately"). The previous implementation retried
                    // in a tight loop without updating `timestamp`, which can spin
                    // forever and look like a deadlock.
                    //
                    // Clamp to `timestamp` to indicate "poll ASAP" without spinning.
                    match interface.poll_at(timestamp, &sockets) {
                        Some(instant) if instant <= timestamp => Some(timestamp),
                        other => other,
                    }
                },
            )
        };

        // Reclaim TCP sockets that have fully closed.
        // Lock order: sockets -> tcp_close_defer (matches close path, which may touch sockets then defer close).
        self.tcp_close_defer
            .reap_closed(&mut sockets, &self.port_manager);

        // drop sockets here to avoid deadlock
        drop(interface);
        drop(sockets);
        // log::info!(
        //     "polling iface {}, has_events: {}, poll_at: {:?}",
        //     self.iface_id,
        //     has_events,
        //     poll_at
        // );

        use core::sync::atomic::Ordering;
        if let Some(instant) = poll_at {
            let _old_instant = self.poll_at_us.load(Ordering::Relaxed);
            let new_instant = instant.total_micros() as u64;
            self.poll_at_us.store(new_instant, Ordering::Relaxed);

            // TODO: poll at
            // if old_instant == 0 || new_instant < old_instant {
            //     self.polling_wait_queue.wake_all();
            // }
        } else {
            self.poll_at_us.store(0, Ordering::Relaxed);
        }

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
        {
            // Avoid allocation here: take one Arc clone at a time, drop the lock, then notify.
            let mut idx = 0usize;
            loop {
                let sock = {
                    let guard = self.bounds.read_irqsave();
                    if idx >= guard.len() {
                        break;
                    }
                    let s = guard[idx].clone();
                    s
                };
                // incase our inet socket missed the event, we manually notify it each time we poll
                sock.notify();
                let _woke = sock.wait_queue().wakeup(Some(ProcessState::Blocked(true)));
                idx += 1;
            }
        }

        // TODO: remove closed sockets
        // let closed_sockets = self
        //     .closing_sockets
        //     .lock_irq_disabled()
        //     .extract_if(|closing_socket| closing_socket.is_closed())
        //     .collect::<Vec<_>>();
        // drop(closed_sockets);
        has_events
    }

    /// 返回 smoltcp 计算的“下次需要 poll 的时间点”（微秒时间戳）。
    ///
    /// - `None` 表示当前没有定时事件需要驱动。
    /// - `Some(us)` 表示应当在 `us` 到达时再次 poll，以推进 TCP 定时器等。
    #[inline]
    pub fn poll_at_us(&self) -> Option<u64> {
        use core::sync::atomic::Ordering;
        let v = self.poll_at_us.load(Ordering::Relaxed);
        if v == 0 {
            None
        } else {
            Some(v)
        }
    }

    /// NAPI 使用的 bounded poll：最多处理 `budget` 个 ingress 包，然后推进一次 egress。
    ///
    /// 返回值语义：是否仍有 ingress backlog 需要继续 poll（即 budget 用尽且仍在处理包）。
    pub fn poll_napi<D>(&self, device: &mut D, budget: usize) -> bool
    where
        D: smoltcp::phy::Device + ?Sized,
    {
        let timestamp = crate::time::Instant::now().into();
        let mut sockets = self.sockets.lock();
        let mut interface = self.smol_iface.lock();

        // 刷新 listener 缓存：必须在持有 sockets 锁的前提下进行，且不得额外分配。
        self.tcp_listener_backlog
            .refresh_listen_socket_present(&sockets);

        let mut processed = 0usize;
        let mut had_packet = false;

        for _ in 0..budget {
            match interface.poll_ingress_single(timestamp, device, &mut sockets) {
                smoltcp::iface::PollIngressSingleResult::None => break,
                smoltcp::iface::PollIngressSingleResult::PacketProcessed => {
                    had_packet = true;
                    processed += 1;
                }
                smoltcp::iface::PollIngressSingleResult::SocketStateChanged => {
                    had_packet = true;
                    processed += 1;
                }
            }
        }

        // 推进发送路径（smoltcp 保证 bounded work）。
        let _ = interface.poll_egress(timestamp, device, &mut sockets);

        // 更新 poll_at（用于定时驱动 TCP）。
        use core::sync::atomic::Ordering;
        let poll_at = match interface.poll_at(timestamp, &sockets) {
            Some(instant) if instant <= timestamp => Some(timestamp),
            other => other,
        };
        if let Some(instant) = poll_at {
            self.poll_at_us
                .store(instant.total_micros() as u64, Ordering::Relaxed);
        } else {
            self.poll_at_us.store(0, Ordering::Relaxed);
        }

        // 解锁后唤醒/通知 socket（沿用原 poll() 的 Linux-like 语义）。
        drop(interface);
        drop(sockets);
        {
            let mut idx = 0usize;
            loop {
                let sock = {
                    let guard = self.bounds.read_irqsave();
                    if idx >= guard.len() {
                        break;
                    }
                    guard[idx].clone()
                };
                sock.notify();
                let _ = sock.wait_queue().wakeup(Some(ProcessState::Blocked(true)));
                idx += 1;
            }
        }

        // NAPI 语义：仅当 ingress backlog 超过 budget 才认为“还有工作没做完”。
        had_packet && processed == budget
    }

    pub fn update_ip_addrs(&self, ip_addrs: &[smoltcp::wire::IpCidr]) -> Result<(), SystemError> {
        if ip_addrs.len() != 1 {
            return Err(SystemError::EINVAL);
        }

        self.smol_iface.lock().update_ip_addrs(|addrs| {
            let dest = addrs.iter_mut().next();

            if let Some(dest) = dest {
                *dest = ip_addrs[0];
            } else {
                addrs.push(ip_addrs[0]).expect("Push ipCidr failed: full");
            }
        });
        return Ok(());
    }

    // 需要bounds储存具体的Inet Socket信息，以提供不同种类inet socket的事件分发
    pub fn bind_socket(&self, socket: Arc<dyn InetSocket>) {
        self.bounds.write().push(socket);
    }

    pub fn unbind_socket(&self, socket: Arc<dyn InetSocket>) {
        let mut bounds = self.bounds.write();
        if let Some(index) = bounds.iter().position(|s| Arc::ptr_eq(s, &socket)) {
            bounds.remove(index);
            // log::debug!("unbind socket success");
        }
    }

    /// Notify all bound sockets unconditionally.
    /// This is used after listener shutdown to ensure all client sockets
    /// are woken up even if the interface poll didn't detect any events.
    pub fn notify_all_bound_sockets(&self) {
        // Avoid allocation and avoid holding bounds lock while notifying.
        let mut idx = 0usize;
        loop {
            let sock = {
                let guard = self.bounds.read_irqsave();
                if idx >= guard.len() {
                    break;
                }
                guard[idx].clone()
            };
            sock.notify();
            let _woke = sock.wait_queue().wakeup(Some(ProcessState::Blocked(true)));
            idx += 1;
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

    pub fn flags(&self) -> InterfaceFlags {
        self.flags
    }

    pub fn type_(&self) -> InterfaceType {
        self.type_
    }

    /// 注册 packet socket 以接收原始数据包
    pub fn register_packet_socket(&self, socket: Weak<PacketSocket>) {
        self.packet_sockets.write().push(socket);
    }

    /// 取消注册 packet socket
    pub fn unregister_packet_socket(&self, socket: &Weak<PacketSocket>) {
        let mut sockets = self.packet_sockets.write();
        sockets.retain(|s| !Weak::ptr_eq(s, socket));
    }

    /// 向所有注册的 packet socket 分发数据包
    ///
    /// # 参数
    /// - `frame`: 完整的以太网帧
    /// - `pkt_type`: 数据包类型
    pub fn deliver_to_packet_sockets(
        &self,
        frame: &[u8],
        pkt_type: crate::net::socket::packet::PacketType,
    ) {
        let sockets = self.packet_sockets.read();
        for socket_weak in sockets.iter() {
            if let Some(socket) = socket_weak.upgrade() {
                socket.deliver_packet(frame, pkt_type);
            }
        }

        // 清理已释放的 weak 引用（延迟清理）
        drop(sockets);
        let mut sockets = self.packet_sockets.write();
        sockets.retain(|s| s.strong_count() > 0);
    }

    /// 发送原始数据包
    ///
    /// 目前是简化实现，后续可以扩展
    pub fn send_raw_packet(&self, _frame: &[u8]) -> Result<(), SystemError> {
        // TODO: 实现原始数据包发送
        // 这需要直接访问网卡驱动的 TX 队列
        // 目前返回 ENOSYS，后续可以扩展
        log::warn!("send_raw_packet: not fully implemented yet");
        Err(SystemError::ENOSYS)
    }
}
