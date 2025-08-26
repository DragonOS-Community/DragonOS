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
use crate::driver::net::register_netdevice;
use crate::driver::net::types::InterfaceFlags;
use crate::filesystem::kernfs::KernFSInode;
use crate::init::initcall::INITCALL_DEVICE;
use crate::libs::rwlock::{RwLockReadGuard, RwLockWriteGuard};
use crate::libs::spinlock::{SpinLock, SpinLockGuard};
use crate::libs::wait_queue::WaitQueue;
use crate::net::generate_iface_id;
use crate::net::routing::{RouteEntry, RouterEnableDevice};
use crate::process::namespace::net_namespace::{NetNamespace, INIT_NET_NAMESPACE};
use crate::process::{ProcessManager, ProcessState};
use crate::sched::SchedMode;
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

    pub fn send_to_peer(&self, data: &Vec<u8>) {
        if let Some(peer) = self.peer.upgrade() {
            // log::info!("Veth {} trying to send", self.name);

            if let Some(bridge_common_data) = peer.inner.lock().bridge_common_data.as_ref() {
                // log::info!("Veth {} sending data to bridge", self.name);
                Self::to_bridge(bridge_common_data, data);
                return;
            }

            // 如果是路由设备，则将数据发送到路由器
            if self.to_router(data) {
                // log::info!("Veth {} sent data to router", self.name);
                return;
            }

            Self::to_peer(&peer, data);
        }
    }

    pub(self) fn to_peer(peer: &Arc<VethInterface>, data: &[u8]) {
        let mut peer_veth = peer.driver.force_get_mut().inner.lock_irqsave();
        peer_veth.rx_queue.push_back(data.to_vec());
        log::info!("Veth {} received data from peer", peer.name);
        log::info!("{:?}", peer_veth.rx_queue);
        drop(peer_veth);

        // 唤醒对端正在等待的进程
        peer.wake_up();

        if let Some(ns) = peer.net_namespace() {
            ns.wakeup_poll_thread();
        }
    }

    fn to_bridge(bridge_data: &BridgeCommonData, data: &Vec<u8>) {
        // log::info!("Veth {} sending data to bridge", self.name);
        let Some(bridge) = bridge_data.bridge_iface.upgrade() else {
            log::warn!("Bridge has been dropped");
            return;
        };
        bridge.driver.enqueue_frame(bridge_data.id, data)
    }

    /// 经过路由发送，返回是否发送成功
    fn to_router(&self, data: &[u8]) -> bool {
        let Some(self_iface) = self.self_iface_ref.upgrade() else {
            return false;
        };

        let frame: EthernetFrame<&[u8]> = EthernetFrame::new_checked(data).unwrap();
        log::info!("trying to go to router");
        match self_iface.handle_routable_packet(&frame) {
            Ok(_) => {
                log::info!("successfully sent to router");
                return true;
            }
            // 先不管错误，直接告诉外面没有经过路由发送出去
            Err(Some(err)) => {
                log::error!("Router error: {:?}", err);
                return false;
            }
            Err(_) => {
                log::info!("not routed");
                return false;
            }
        }
    }

    pub fn recv_from_peer(&mut self) -> Option<Vec<u8>> {
        // log::info!("Veth {} trying to receive", self.name);
        self.rx_queue.pop_front()
    }

    pub fn name(&self) -> &str {
        &self.name
    }
}

#[derive(Clone)]
pub struct VethDriver {
    pub inner: Arc<SpinLock<Veth>>,
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

        let driver1 = VethDriver { inner: dev1 };
        let driver2 = VethDriver { inner: dev2 };

        (driver1, driver2)
    }

    pub fn name(&self) -> String {
        self.inner.lock_irqsave().name().to_string()
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
        self.driver.inner.lock_irqsave().send_to_peer(&buf);
        result
    }
}

pub struct VethRxToken {
    buffer: Vec<u8>,
}

impl RxToken for VethRxToken {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        f(&self.buffer)
    }
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
        let mut guard = self.inner.lock_irqsave();
        guard.recv_from_peer().map(|buf| {
            // log::info!("VethDriver received data: {:?}", buf);
            (
                VethRxToken { buffer: buf },
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
    wait_queue: WaitQueue,
}

#[derive(Debug)]
pub struct VethCommonData {
    netdevice_common: NetDeviceCommonData,
    device_common: DeviceCommonData,
    kobj_common: KObjectCommonData,
    peer_veth: Weak<VethInterface>,

    bridge_common_data: Option<BridgeCommonData>,
}

impl Default for VethCommonData {
    fn default() -> Self {
        VethCommonData {
            netdevice_common: NetDeviceCommonData::default(),
            device_common: DeviceCommonData::default(),
            kobj_common: KObjectCommonData::default(),
            peer_veth: Weak::new(),
            bridge_common_data: None,
        }
    }
}

impl VethInterface {
    pub fn has_data(&self) -> bool {
        let driver = self.driver.force_get_mut();
        let inner = driver.inner.lock_irqsave();
        !inner.rx_queue.is_empty()
    }

    #[allow(unused)]
    pub fn peer_veth(&self) -> Arc<VethInterface> {
        self.inner.lock_irqsave().peer_veth.upgrade().unwrap()
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
            common: IfaceCommon::new(
                iface_id,
                super::types::InterfaceType::EETHER,
                flags,
                false,
                iface,
            ),
            inner: SpinLock::new(VethCommonData::default()),
            locked_kobj_state: LockedKObjectState::default(),
            wait_queue: WaitQueue::default(),
        });

        driver.inner.lock().self_iface_ref = Arc::downgrade(&device);

        // log::info!("VethInterface {} created with ID {}", device.name, iface_id);
        device
    }

    pub fn set_peer_iface(&self, peer: &Arc<VethInterface>) {
        let mut inner = self.inner.lock_irqsave();
        inner.peer_veth = Arc::downgrade(peer);
        self.driver.inner.lock_irqsave().set_peer_iface(peer);
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

    pub fn wake_up(&self) {
        self.wait_queue.wakeup(Some(ProcessState::Blocked(true)));
    }
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
    fn kobj_state(&self) -> RwLockReadGuard<'_, KObjectState> {
        self.locked_kobj_state.read()
    }

    fn kobj_state_mut(&self) -> RwLockWriteGuard<'_, KObjectState> {
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

    fn poll_blocking(&self, can_stop_fn: &dyn Fn() -> bool) {
        log::info!("VethInterface {} polling block", self.name);

        loop {
            // 检查是否有数据可用
            self.common.poll(self.driver.force_get_mut());

            let has_data = self.has_data();

            // 外部 socket 是否可以接收数据，如果是的话就可以退出loop了
            let can_stop = can_stop_fn();

            if can_stop {
                break;
            }

            // 没有数据可用时，进入等待队列
            // 如果有数据可用，则直接跳出循环
            log::info!("VethInterface {} waiting for data", self.name);
            if !has_data {
                let _ = wq_wait_event_interruptible!(
                    self.wait_queue,
                    self.has_data() || can_stop_fn(),
                    {}
                );
            }
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
        log::info!("VethInterface {} received from bridge", self.name);
        let peer = self.peer_veth();

        // let inner = self.inner.lock_irqsave();

        if self
            .inner
            .lock_irqsave()
            .bridge_common_data
            .as_ref()
            .unwrap()
            .bridge_iface
            .upgrade()
            .is_some()
        {
            log::info!("VethInterface {} sending data to peer", self.name);

            // let peer = self.peer_veth();
            Veth::to_peer(&peer, frame);
            // self.driver
            //     .inner
            //     .lock_irqsave()
            //     .rx_queue
            //     .push_back(frame.to_vec());
            // peer.poll();
        }
        log::info!("returning");
    }

    fn set_common_bridge_data(&self, port: BridgePort) {
        // log::info!("Now set bridge port data for {}", self.name);
        let mut inner = self.inner.lock_irqsave();
        let data = BridgeCommonData {
            id: port.id,
            bridge_iface: port.bridge_iface.clone(),
        };
        inner.bridge_common_data = Some(data);
    }

    // fn common_bridge_data(&self) -> Option<BridgeCommonData> {
    //     self.inner().bridge_common_data.clone()
    // }
}

impl RouterEnableDevice for VethInterface {
    fn route_and_send(&self, next_hop: IpAddress, ip_packet: &[u8]) {
        log::info!(
            "VethInterface {} routing packet to {}",
            self.iface_name(),
            next_hop
        );

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
        self.driver
            .force_get_mut()
            .inner
            .lock_irqsave()
            .send_to_peer(&frame);
    }

    fn is_my_ip(&self, ip: IpAddress) -> bool {
        self.common
            .ip_addrs()
            .iter()
            .any(|cidr| cidr.contains_addr(&ip))
    }
}

// pub fn veth_probe(name1: &str, name2: &str) -> (Arc<VethInterface>, Arc<VethInterface>) {
//     let (iface1, iface2) = VethInterface::new_pair(name1, name2);

//     let addr1 = IpAddress::v4(10, 0, 0, 1);
//     let cidr1 = IpCidr::new(addr1, 24);
//     iface1.update_ip_addrs(cidr1);

//     let addr2 = IpAddress::v4(10, 0, 0, 2);
//     let cidr2 = IpCidr::new(addr2, 24);
//     iface2.update_ip_addrs(cidr2);

//     // 添加默认路由
//     iface1.add_default_route_to_peer(addr2);
//     iface2.add_default_route_to_peer(addr1);

//     let turn_on = |a: &Arc<VethInterface>| {
//         a.set_net_state(NetDeivceState::__LINK_STATE_START);
//         a.set_operstate(Operstate::IF_OPER_UP);
//         // NET_DEVICES.write_irqsave().insert(a.nic_id(), a.clone());
//         INIT_NET_NAMESPACE.add_device(a.clone());
//         a.common().set_net_namespace(INIT_NET_NAMESPACE.clone());
//         register_netdevice(a.clone()).expect("register veth device failed");
//     };

//     turn_on(&iface1);
//     turn_on(&iface2);

//     (iface1, iface2)
// }

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

    let addr4 = IpAddress::v4(192, 168, 2, 1);
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
}

#[unified_init(INITCALL_DEVICE)]
pub fn veth_init() -> Result<(), SystemError> {
    // veth_probe("veth0", "veth1");
    veth_route_test();
    log::info!("Veth pair initialized.");
    Ok(())
}
