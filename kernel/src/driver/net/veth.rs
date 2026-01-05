use super::bridge::BridgeEnableDevice;
use super::{Iface, IfaceCommon};
use super::{NetDeivceState, NetDeviceCommonData, Operstate};
use crate::arch::rand::rand;
use crate::driver::base::class::Class;
use crate::driver::base::device::bus::Bus;
use crate::driver::base::device::driver::Driver;
use crate::driver::base::device::{self, DeviceCommonData, DeviceType, IdTable};
use crate::driver::base::kobject::{
    KObjType, KObject, KObjectCommonData, KObjectState, LockedKObjectState,
};
use crate::driver::base::kset::KSet;
use crate::driver::net::bridge::{BridgeCommonData, BridgePort};
use crate::driver::net::napi::{napi_schedule, NapiStruct};
use crate::driver::net::register_netdevice;
use crate::driver::net::types::InterfaceFlags;
use crate::filesystem::kernfs::KernFSInode;
use crate::init::initcall::INITCALL_DEVICE;
use crate::libs::rwsem::{RwSemReadGuard, RwSemWriteGuard};
use crate::libs::spinlock::{SpinLock, SpinLockGuard};
use crate::net::generate_iface_id;
use crate::net::routing::{DnatRule, RouteEntry, RouterEnableDevice, SnatRule};
use crate::process::namespace::net_namespace::{NetNamespace, INIT_NET_NAMESPACE};
use crate::process::ProcessManager;
use alloc::collections::VecDeque;
use alloc::fmt::Debug;
use alloc::string::{String, ToString};
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use smoltcp::phy::DeviceCapabilities;
use smoltcp::phy::{self, RxToken};
use smoltcp::wire::{EthernetAddress, EthernetFrame, HardwareAddress, IpAddress, IpCidr};
use system_error::SystemError;
use unified_init::macros::unified_init;

pub struct Veth {
    name: String,
    rx_queue: VecDeque<Vec<u8>>,
    /// 对端的 `VethInterface`，在完成数据发送的时候会使用到
    peer: Weak<VethInterface>,
    self_iface_ref: Weak<VethInterface>,
}

impl Veth {
    pub fn new(name: String) -> Self {
        Veth {
            name,
            rx_queue: VecDeque::new(),
            peer: Weak::new(),
            self_iface_ref: Weak::new(),
        }
    }

    pub fn set_peer_iface(&mut self, peer: &Arc<VethInterface>) {
        self.peer = Arc::downgrade(peer);
    }

    pub fn send_to_peer(&self, data: &[u8]) {
        if let Some(peer) = self.peer.upgrade() {
            // log::info!("Veth {} trying to send", self.name);

            Self::to_peer(&peer, data);
        }
    }

    pub(self) fn to_peer(peer: &Arc<VethInterface>, data: &[u8]) {
        let mut peer_veth = peer.driver.inner.lock();
        peer_veth.rx_queue.push_back(data.to_vec());

        // {
        //     let ether = EthernetFrame::new_checked(data).unwrap();
        //     if ether.ethertype() == smoltcp::wire::EthernetProtocol::Ipv4 {
        //         if let Some(ipv4_packet) =
        //             smoltcp::wire::Ipv4Packet::new_checked(ether.payload()).ok()
        //         {
        //             log::info!(
        //                 "Veth {} sending IPv4 packet to peer: {} -> {}",
        //                 peer.name,
        //                 ipv4_packet.src_addr(),
        //                 ipv4_packet.dst_addr()
        //             );
        //         }
        //     } else if ether.ethertype() == smoltcp::wire::EthernetProtocol::Ipv6 {
        //         if let Some(ipv6_packet) =
        //             smoltcp::wire::Ipv6Packet::new_checked(ether.payload()).ok()
        //         {
        //             log::info!(
        //                 "Veth {} sending IPv6 packet to peer: {} -> {}",
        //                 peer.name,
        //                 ipv6_packet.src_addr(),
        //                 ipv6_packet.dst_addr()
        //             );
        //         }
        //     } else {
        //         log::info!(
        //             "Veth {} sending non-IP packet to peer: ethertype={:?}",
        //             peer.name,
        //             ether.ethertype()
        //         );
        //     }
        // }

        drop(peer_veth);

        let Some(napi) = peer.napi_struct() else {
            log::error!("Veth {} has no napi_struct", peer.name);
            return;
        };

        napi_schedule(napi);
    }

    fn to_bridge(bridge_data: &BridgeCommonData, data: &[u8]) {
        // log::info!("Veth {} sending data to bridge", self.name);
        let Some(bridge) = bridge_data.bridge_driver_ref.upgrade() else {
            log::warn!("Bridge has been dropped");
            return;
        };
        bridge.handle_frame(bridge_data.id, data);
    }

    /// 经过路由发送，返回是否发送成功
    fn to_router(&self, data: &[u8]) -> bool {
        let Some(self_iface) = self.self_iface_ref.upgrade() else {
            return false;
        };

        let frame: EthernetFrame<&[u8]> = EthernetFrame::new_checked(data).unwrap();
        // log::info!("trying to go to router");
        match self_iface.handle_routable_packet(&frame) {
            Ok(_) => {
                // log::info!("successfully sent to router");
                true
            }
            // 先不管错误，直接告诉外面没有经过路由发送出去
            Err(Some(err)) => {
                log::error!("Router error: {:?}", err);
                false
            }
            Err(_) => {
                // log::info!("not routed");
                false
            }
        }
    }

    pub fn recv_from_peer(&mut self) -> Option<Vec<u8>> {
        // log::info!("Veth {} trying to receive", self.name);
        let data = self.rx_queue.pop_front()?;

        if let Some(bridge_common_data) = self
            .self_iface_ref
            .upgrade()
            .unwrap()
            .inner
            .lock()
            .bridge_common_data
            .as_ref()
        {
            // log::info!("Veth {} sending data to bridge", self.name);
            Self::to_bridge(bridge_common_data, &data);
            return None;
        }

        // 说明获取的包发给进入路由了，无须返回
        if self.to_router(&data) {
            return None;
        }

        Some(data)
    }

    pub fn name(&self) -> &str {
        &self.name
    }
}

pub struct VethDriver {
    pub inner: Arc<SpinLock<Veth>>,
    /// 指向所属网络接口的弱引用，用于 packet socket 分发
    iface: SpinLock<Weak<dyn Iface>>,
}

impl Clone for VethDriver {
    fn clone(&self) -> Self {
        VethDriver {
            inner: self.inner.clone(),
            iface: SpinLock::new(self.iface.lock().clone()),
        }
    }
}

impl VethDriver {
    /// # `new_pair`
    /// 创建一对虚拟以太网设备（veth pair），用于网络测试
    /// ## 参数
    /// - `name1`: 第一个设备的名称
    /// - `name2`: 第二个设备的名称
    /// ## 返回值
    /// 返回一个元组，包含两个 `VethDriver` 实例，分别对应
    /// 第一个和第二个虚拟以太网设备。
    pub fn new_pair(name1: &str, name2: &str) -> (VethDriver, VethDriver) {
        let dev1 = Arc::new(SpinLock::new(Veth::new(name1.to_string())));
        let dev2 = Arc::new(SpinLock::new(Veth::new(name2.to_string())));

        let driver1 = VethDriver {
            inner: dev1,
            iface: SpinLock::new(Weak::<VethInterface>::new()),
        };
        let driver2 = VethDriver {
            inner: dev2,
            iface: SpinLock::new(Weak::<VethInterface>::new()),
        };

        (driver1, driver2)
    }

    pub fn name(&self) -> String {
        self.inner.lock().name().to_string()
    }

    /// 设置所属网络接口的引用
    pub fn set_iface(&self, iface: Weak<dyn Iface>) {
        *self.iface.lock() = iface;
    }

    /// 获取所属网络接口
    pub fn iface(&self) -> Option<Arc<dyn Iface>> {
        self.iface.lock().upgrade()
    }
}

pub struct VethTxToken {
    driver: VethDriver,
}

impl phy::TxToken for VethTxToken {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let mut buf = vec![0; len];
        let result = f(&mut buf);
        self.driver.inner.lock().send_to_peer(&buf);
        result
    }
}

pub struct VethRxToken {
    buffer: Vec<u8>,
    driver: VethDriver,
}

impl RxToken for VethRxToken {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        let packet = self.buffer.as_slice();

        // 向注册的 packet socket 分发数据包
        if let Some(iface) = self.driver.iface() {
            let pkt_type = determine_packet_type(packet, &iface);
            iface.common().deliver_to_packet_sockets(packet, pkt_type);
        }

        f(packet)
    }
}

/// 根据以太网帧的目的 MAC 地址确定数据包类型
fn determine_packet_type(
    frame: &[u8],
    iface: &Arc<dyn Iface>,
) -> crate::net::socket::packet::PacketType {
    use crate::net::socket::packet::PacketType;

    if frame.len() < 14 {
        return PacketType::Host;
    }

    let dst_mac = &frame[0..6];

    // 检查是否为广播地址 (FF:FF:FF:FF:FF:FF)
    if dst_mac == [0xff, 0xff, 0xff, 0xff, 0xff, 0xff] {
        return PacketType::Broadcast;
    }

    // 检查是否为多播地址 (第一个字节的最低位为1)
    if dst_mac[0] & 0x01 != 0 {
        return PacketType::Multicast;
    }

    // 检查是否为发往本机的包
    let our_mac = iface.mac();
    if dst_mac == our_mac.as_bytes() {
        return PacketType::Host;
    }

    // 其他情况为发往其他主机的包（混杂模式下捕获）
    PacketType::OtherHost
}

#[derive(Debug)]
struct VethDriverWarpper(UnsafeCell<VethDriver>);
unsafe impl Send for VethDriverWarpper {}
unsafe impl Sync for VethDriverWarpper {}

impl Deref for VethDriverWarpper {
    type Target = VethDriver;
    fn deref(&self) -> &Self::Target {
        unsafe { &*self.0.get() }
    }
}

impl DerefMut for VethDriverWarpper {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { &mut *self.0.get() }
    }
}

impl VethDriverWarpper {
    #[allow(clippy::mut_from_ref)]
    fn force_get_mut(&self) -> &mut VethDriver {
        unsafe { &mut *self.0.get() }
    }
}

impl phy::Device for VethDriver {
    type RxToken<'a> = VethRxToken;
    type TxToken<'a> = VethTxToken;

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.max_transmission_unit = 1500;
        caps.medium = smoltcp::phy::Medium::Ethernet;
        caps
    }

    fn receive(
        &mut self,
        _timestamp: smoltcp::time::Instant,
    ) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        let mut guard = self.inner.lock();
        guard.recv_from_peer().map(|buf| {
            // log::info!("VethDriver received data: {:?}", buf);
            (
                VethRxToken {
                    buffer: buf,
                    driver: self.clone(),
                },
                VethTxToken {
                    driver: self.clone(),
                },
            )
        })
    }

    fn transmit(&mut self, _timestamp: smoltcp::time::Instant) -> Option<Self::TxToken<'_>> {
        Some(VethTxToken {
            driver: self.clone(),
        })
    }
}

#[cast_to([sync] Iface)]
#[cast_to([sync] device::Device)]
#[derive(Debug)]
pub struct VethInterface {
    name: String,
    driver: VethDriverWarpper,
    common: IfaceCommon,
    inner: SpinLock<VethCommonData>,
    locked_kobj_state: LockedKObjectState,
}

#[derive(Debug, Default)]
pub struct VethCommonData {
    netdevice_common: NetDeviceCommonData,
    device_common: DeviceCommonData,
    kobj_common: KObjectCommonData,
    peer_veth: Weak<VethInterface>,

    bridge_common_data: Option<BridgeCommonData>,
}

impl VethInterface {
    pub fn peer_veth(&self) -> Arc<VethInterface> {
        self.inner.lock().peer_veth.upgrade().unwrap()
    }

    pub fn new(driver: VethDriver) -> Arc<Self> {
        let iface_id = generate_iface_id();
        let name = driver.name();
        let mac = [
            0x02,
            0x00,
            0x00,
            0x00,
            (iface_id >> 8) as u8,
            iface_id as u8,
        ];
        let hw_addr = HardwareAddress::Ethernet(EthernetAddress(mac));
        let mut iface_config = smoltcp::iface::Config::new(hw_addr);
        iface_config.random_seed = rand() as u64;
        let mut iface = smoltcp::iface::Interface::new(
            iface_config,
            &mut driver.clone(),
            crate::time::Instant::now().into(),
        );
        iface.set_any_ip(true);

        let flags = InterfaceFlags::BROADCAST
            | InterfaceFlags::MULTICAST
            | InterfaceFlags::UP
            | InterfaceFlags::RUNNING
            | InterfaceFlags::LOWER_UP;

        let device = Arc::new(VethInterface {
            name,
            driver: VethDriverWarpper(UnsafeCell::new(driver.clone())),
            common: IfaceCommon::new(iface_id, super::types::InterfaceType::EETHER, flags, iface),
            inner: SpinLock::new(VethCommonData::default()),
            locked_kobj_state: LockedKObjectState::default(),
        });
        let napi_struct = NapiStruct::new(device.clone(), 10);
        *device.common.napi_struct.write() = Some(napi_struct);

        // 设置 driver 对接口的弱引用，用于 packet socket 分发
        device
            .driver
            .force_get_mut()
            .set_iface(Arc::downgrade(&device) as Weak<dyn Iface>);

        driver.inner.lock().self_iface_ref = Arc::downgrade(&device);

        // log::info!("VethInterface {} created with ID {}", device.name, iface_id);
        device
    }

    pub fn set_peer_iface(&self, peer: &Arc<VethInterface>) {
        let mut inner = self.inner.lock();
        inner.peer_veth = Arc::downgrade(peer);
        self.driver.inner.lock().set_peer_iface(peer);
    }

    pub fn new_pair(name1: &str, name2: &str) -> (Arc<Self>, Arc<Self>) {
        let (driver1, driver2) = VethDriver::new_pair(name1, name2);
        let iface1 = VethInterface::new(driver1);
        let iface2 = VethInterface::new(driver2);

        iface1.set_peer_iface(&iface2);
        iface2.set_peer_iface(&iface1);

        (iface1, iface2)
    }

    fn inner(&self) -> SpinLockGuard<'_, VethCommonData> {
        self.inner.lock()
    }

    /// # `update_ip_addrs`
    /// 更新虚拟以太网设备的 IP 地址
    /// ## 参数
    /// - `cidr`: 要添加的 IP 地址和子网掩码
    /// ## 描述
    /// 该方法会将指定的 IP 地址添加到虚拟以太网设备的 IP 地址列表中。
    /// 如果添加失败（例如列表已满），则会触发 panic。
    pub fn update_ip_addrs(&self, cidr: IpCidr) {
        let iface = &mut self.common.smol_iface.lock_irqsave();
        iface.update_ip_addrs(|ip_addrs| {
            ip_addrs.push(cidr).expect("Push ipCidr failed: full");
        });
        self.common.router_common_data.ip_addrs.write().push(cidr);

        // // 直接更新对端的arp_table
        // self.inner.lock().peer_veth.upgrade().map(|peer| {
        //     peer.common
        //         .router_common_data
        //         .arp_table
        //         .write()
        //         .insert(cidr.address(), self.mac())
        // });

        // log::info!("VethInterface {} updated IP address: {}", self.name, addr);
    }

    /// # `add_default_route_to_peer`
    /// 添加默认路由到对端虚拟以太网设备
    /// ## 参数
    /// - `peer_ip`: 对端设备的 IP 地址
    /// ## 描述
    /// 该方法会在当前虚拟以太网设备的路由表中
    /// 添加一条默认路由，
    /// 指向对端虚拟以太网设备的 IP 地址。
    /// 如果添加失败，则会触发 panic。
    ///
    pub fn add_default_route_to_peer(&self, peer_ip: IpAddress) {
        let iface = &mut self.common.smol_iface.lock_irqsave();
        // iface.update_ip_addrs(|ip_addrs| {
        //     ip_addrs.push(self_cidr).expect("Push ipCidr failed: full");
        // });
        iface.routes_mut().update(|routes_map| {
            routes_map
                .push(smoltcp::iface::Route {
                    cidr: IpCidr::new(IpAddress::v4(0, 0, 0, 0), 0),
                    via_router: peer_ip,
                    preferred_until: None,
                    expires_at: None,
                })
                .expect("Add default route to peer failed");
        });
    }

    // pub fn add_direct_route(&self, cidr: IpCidr, via_router: IpAddress) {
    //     let iface = &mut self.common.smol_iface.lock_irqsave();
    //     iface.routes_mut().update(|routes_map| {
    //         routes_map
    //             .push(smoltcp::iface::Route {
    //                 cidr,
    //                 via_router,
    //                 preferred_until: None,
    //                 expires_at: None,
    //             })
    //             .expect("Add direct route failed");
    //     });
    // }
}

impl KObject for VethInterface {
    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn set_inode(&self, inode: Option<Arc<KernFSInode>>) {
        self.inner().kobj_common.kern_inode = inode;
    }

    fn inode(&self) -> Option<Arc<KernFSInode>> {
        self.inner().kobj_common.kern_inode.clone()
    }

    fn parent(&self) -> Option<Weak<dyn KObject>> {
        self.inner().kobj_common.parent.clone()
    }

    fn set_parent(&self, parent: Option<Weak<dyn KObject>>) {
        self.inner().kobj_common.parent = parent;
    }

    fn kset(&self) -> Option<Arc<KSet>> {
        self.inner().kobj_common.kset.clone()
    }

    fn set_kset(&self, kset: Option<Arc<KSet>>) {
        self.inner().kobj_common.kset = kset;
    }

    fn kobj_type(&self) -> Option<&'static dyn KObjType> {
        self.inner().kobj_common.kobj_type
    }

    fn name(&self) -> String {
        self.name.clone()
    }

    fn set_name(&self, _name: String) {}
    fn kobj_state(&self) -> RwSemReadGuard<'_, KObjectState> {
        self.locked_kobj_state.read()
    }

    fn kobj_state_mut(&self) -> RwSemWriteGuard<'_, KObjectState> {
        self.locked_kobj_state.write()
    }

    fn set_kobj_state(&self, state: KObjectState) {
        *self.locked_kobj_state.write() = state;
    }

    fn set_kobj_type(&self, ktype: Option<&'static dyn KObjType>) {
        self.inner().kobj_common.kobj_type = ktype;
    }
}

impl device::Device for VethInterface {
    fn dev_type(&self) -> DeviceType {
        DeviceType::Net
    }

    fn id_table(&self) -> IdTable {
        IdTable::new(self.name.clone(), None)
    }

    fn bus(&self) -> Option<Weak<dyn Bus>> {
        self.inner().device_common.bus.clone()
    }

    fn set_bus(&self, bus: Option<Weak<dyn Bus>>) {
        self.inner().device_common.bus = bus;
    }

    fn class(&self) -> Option<Arc<dyn Class>> {
        let mut guard = self.inner();
        let r = guard.device_common.class.clone()?.upgrade();
        if r.is_none() {
            guard.device_common.class = None;
        }
        r
    }

    fn set_class(&self, class: Option<Weak<dyn Class>>) {
        self.inner().device_common.class = class;
    }

    fn driver(&self) -> Option<Arc<dyn Driver>> {
        let r = self.inner().device_common.driver.clone()?.upgrade();
        if r.is_none() {
            self.inner().device_common.driver = None;
        }
        r
    }

    fn set_driver(&self, driver: Option<Weak<dyn Driver>>) {
        self.inner().device_common.driver = driver;
    }

    fn is_dead(&self) -> bool {
        false
    }

    fn can_match(&self) -> bool {
        self.inner().device_common.can_match
    }

    fn set_can_match(&self, can_match: bool) {
        self.inner().device_common.can_match = can_match;
    }

    fn state_synced(&self) -> bool {
        true
    }

    fn dev_parent(&self) -> Option<Weak<dyn device::Device>> {
        self.inner().device_common.get_parent_weak_or_clear()
    }

    fn set_dev_parent(&self, parent: Option<Weak<dyn device::Device>>) {
        self.inner().device_common.parent = parent;
    }
}

impl Iface for VethInterface {
    fn common(&self) -> &IfaceCommon {
        &self.common
    }

    fn iface_name(&self) -> String {
        self.name.clone()
    }

    fn mac(&self) -> EthernetAddress {
        if let HardwareAddress::Ethernet(mac) =
            self.common.smol_iface.lock_irqsave().hardware_addr()
        {
            mac
        } else {
            EthernetAddress([0, 0, 0, 0, 0, 0])
        }
    }

    fn poll(&self) -> bool {
        // log::info!("VethInterface {} polling normal", self.name);
        self.common.poll(self.driver.force_get_mut())
        // self.clear_recv_buffer();
    }

    fn addr_assign_type(&self) -> u8 {
        self.inner().netdevice_common.addr_assign_type
    }

    fn net_device_type(&self) -> u16 {
        self.inner().netdevice_common.net_device_type = 1;
        self.inner().netdevice_common.net_device_type
    }

    fn net_state(&self) -> NetDeivceState {
        self.inner().netdevice_common.state
    }

    fn set_net_state(&self, state: NetDeivceState) {
        self.inner().netdevice_common.state |= state;
    }

    fn operstate(&self) -> Operstate {
        self.inner().netdevice_common.operstate
    }

    fn set_operstate(&self, state: Operstate) {
        self.inner().netdevice_common.operstate = state;
    }

    fn mtu(&self) -> usize {
        use smoltcp::phy::Device;
        self.driver
            .force_get_mut()
            .capabilities()
            .max_transmission_unit
    }
}

impl BridgeEnableDevice for VethInterface {
    fn receive_from_bridge(&self, frame: &[u8]) {
        // log::info!("VethInterface {} received from bridge", self.name);
        let peer = self.peer_veth();

        if self
            .inner
            .lock()
            .bridge_common_data
            .as_ref()
            .unwrap()
            .bridge_driver_ref
            .upgrade()
            .is_some()
        {
            // log::info!("VethInterface {} sending data to peer", self.name);

            // let peer = self.peer_veth();
            Veth::to_peer(&peer, frame);
        }
    }

    fn set_common_bridge_data(&self, port: &BridgePort) {
        // log::info!("Now set bridge port data for {}", self.name);
        let mut inner = self.inner.lock();
        let data = BridgeCommonData {
            id: port.id,
            bridge_driver_ref: port.bridge_driver_ref.clone(),
        };
        inner.bridge_common_data = Some(data);
    }

    fn common_bridge_data(&self) -> Option<BridgeCommonData> {
        self.inner().bridge_common_data.clone()
    }
}

impl RouterEnableDevice for VethInterface {
    fn route_and_send(&self, _next_hop: &IpAddress, ip_packet: &[u8]) {
        // log::info!(
        //     "VethInterface {} routing packet to {}",
        //     self.iface_name(),
        //     next_hop
        // );

        // 构造以太网帧
        let dst_mac = self.peer_veth().mac();
        let src_mac = self.mac();

        // 以太网类型为 IPv4
        let ethertype = [0x08, 0x00];

        let mut frame = Vec::with_capacity(14 + ip_packet.len());
        frame.extend_from_slice(&dst_mac.0);
        frame.extend_from_slice(&src_mac.0);
        frame.extend_from_slice(&ethertype);
        frame.extend_from_slice(ip_packet);

        // 发送到对端
        self.driver.inner.lock().send_to_peer(&frame);
    }

    fn is_my_ip(&self, ip: IpAddress) -> bool {
        self.common
            .ip_addrs()
            .iter()
            .any(|cidr| cidr.contains_addr(&ip))
    }
}

fn veth_route_test() {
    let (iface_ns1, iface_host1) = VethInterface::new_pair("veth-ns1", "veth-host1");
    let (iface_ns2, iface_host2) = VethInterface::new_pair("veth-ns2", "veth-host2");

    let addr1 = IpAddress::v4(192, 168, 1, 1);
    let cidr1 = IpCidr::new(addr1, 24);
    iface_ns1.update_ip_addrs(cidr1);

    let addr2 = IpAddress::v4(192, 168, 1, 254);
    let cidr2 = IpCidr::new(addr2, 24);
    iface_host1.update_ip_addrs(cidr2);

    let addr3 = IpAddress::v4(192, 168, 2, 254);
    let cidr3 = IpCidr::new(addr3, 24);
    iface_host2.update_ip_addrs(cidr3);

    let addr4 = IpAddress::v4(192, 168, 2, 3);
    let cidr4 = IpCidr::new(addr4, 24);
    iface_ns2.update_ip_addrs(cidr4);

    // 添加默认路由
    iface_ns1.add_default_route_to_peer(addr2);
    iface_host1.add_default_route_to_peer(addr1);

    iface_host2.add_default_route_to_peer(addr4);
    iface_ns2.add_default_route_to_peer(addr3);

    let turn_on = |a: &Arc<VethInterface>, ns: Arc<NetNamespace>| {
        a.set_net_state(NetDeivceState::__LINK_STATE_START);
        a.set_operstate(Operstate::IF_OPER_UP);
        // NET_DEVICES.write_irqsave().insert(a.nic_id(), a.clone());
        ns.add_device(a.clone());
        a.common().set_net_namespace(ns.clone());
        register_netdevice(a.clone()).expect("register veth device failed");
    };

    let ns1 = NetNamespace::new_empty(ProcessManager::current_user_ns()).unwrap();
    let ns2 = NetNamespace::new_empty(ProcessManager::current_user_ns()).unwrap();

    let router_ns1 = ns1.router();
    // 任何发往 192.168.1.0/24 网络的数据包都是本地邻居，可以直接从 veth-ns1 发送。
    let dest = IpCidr::new(IpAddress::v4(192, 168, 1, 0), 24);
    let route = RouteEntry::new_connected(dest, iface_ns1.clone());
    router_ns1.add_route(route);
    // 任何不匹配其他路由的数据包，都应该通过 veth-ns1 接口发送给下一跳 192.168.1.254。
    let next_hop = IpAddress::v4(192, 168, 1, 254);
    let route = RouteEntry::new_default(next_hop, iface_ns1.clone());
    router_ns1.add_route(route);

    let router_ns2 = ns2.router();
    // 任何发往 192.168.2.0/24 网络的数据包都是本地邻居，可以直接从 veth-ns2 发送
    let dest = IpCidr::new(IpAddress::v4(192, 168, 2, 0), 24);
    let route = RouteEntry::new_connected(dest, iface_ns2.clone());
    router_ns2.add_route(route);
    // 任何不匹配其他路由的数据包，都应该通过 veth-ns2 接口发送给下一跳 192.168.2.254
    let next_hop = IpAddress::v4(192, 168, 2, 254);
    let route = RouteEntry::new_default(next_hop, iface_ns2.clone());
    router_ns2.add_route(route);

    let host_router = INIT_NET_NAMESPACE.router();
    // 任何发往 192.168.1.0/24 网络的数据包，都应该从 veth-host1 接口直接发送
    let dest = IpCidr::new(IpAddress::v4(192, 168, 1, 0), 24);
    let route = RouteEntry::new_connected(dest, iface_host1.clone());
    host_router.add_route(route);
    // 任何发往 192.168.2.0/24 网络的数据包，都应该从 veth-host2 接口直接发送
    let dest = IpCidr::new(IpAddress::v4(192, 168, 2, 0), 24);
    let route = RouteEntry::new_connected(dest, iface_host2.clone());
    host_router.add_route(route);

    turn_on(&iface_ns1, INIT_NET_NAMESPACE.clone());
    turn_on(&iface_ns2, INIT_NET_NAMESPACE.clone());
    turn_on(&iface_host1, INIT_NET_NAMESPACE.clone());
    turn_on(&iface_host2, INIT_NET_NAMESPACE.clone());

    let snat_rules = vec![SnatRule {
        // 匹配所有来自 192.168.1.0/24 网络的流量
        source_cidr: "192.168.1.0/24".parse().unwrap(),
        // 将源地址转换为 192.168.2.254(hardcode)
        nat_ip: IpAddress::v4(192, 168, 2, 254),
    }];

    let dnat_rules = vec![DnatRule {
        external_addr: IpAddress::v4(192, 168, 2, 1),
        internal_addr: IpAddress::v4(192, 168, 2, 3),
        internal_port: None,
        external_port: None,
    }];

    host_router.nat_tracker().update_snat_rules(snat_rules);
    host_router.nat_tracker().update_dnat_rules(dnat_rules);
}

fn veth_epoll_test() {
    let (iface1, iface2) = VethInterface::new_pair("veth1", "veth2");

    let addr1 = IpAddress::v4(111, 111, 11, 1);
    let cidr1 = IpCidr::new(addr1, 24);
    iface1.update_ip_addrs(cidr1);

    let addr2 = IpAddress::v4(111, 111, 11, 2);
    let cidr2 = IpCidr::new(addr2, 24);
    iface2.update_ip_addrs(cidr2);

    iface1.add_default_route_to_peer(addr2);
    iface2.add_default_route_to_peer(addr1);

    let turn_on = |a: &Arc<VethInterface>, ns: Arc<NetNamespace>| {
        a.set_net_state(NetDeivceState::__LINK_STATE_START);
        a.set_operstate(Operstate::IF_OPER_UP);
        // NET_DEVICES.write_irqsave().insert(a.nic_id(), a.clone());
        ns.add_device(a.clone());
        a.common().set_net_namespace(ns.clone());
        register_netdevice(a.clone()).expect("register veth device failed");
    };

    turn_on(&iface1, INIT_NET_NAMESPACE.clone());
    turn_on(&iface2, INIT_NET_NAMESPACE.clone());
}

#[unified_init(INITCALL_DEVICE)]
pub fn veth_init() -> Result<(), SystemError> {
    veth_epoll_test();
    veth_route_test();
    log::info!("Veth pair initialized.");
    Ok(())
}
