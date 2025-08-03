use super::{Iface, IfaceCommon};
use super::{NetDeivceState, NetDeviceCommonData, Operstate};
use crate::arch::rand::rand;
use crate::driver::base::class::Class;
use crate::driver::base::device::bus::Bus;
use crate::driver::base::device::driver::Driver;
use crate::driver::base::device::{Device, DeviceCommonData, DeviceType, IdTable};
use crate::driver::base::kobject::{
    KObjType, KObject, KObjectCommonData, KObjectState, LockedKObjectState,
};
use crate::driver::base::kset::KSet;
use crate::filesystem::kernfs::KernFSInode;
use crate::libs::rwlock::{RwLockReadGuard, RwLockWriteGuard};
use crate::libs::spinlock::{SpinLock, SpinLockGuard};
use crate::net::generate_iface_id;
use crate::net::routing::router::Router;
use alloc::collections::VecDeque;
use alloc::fmt::Debug;
use alloc::string::{String, ToString};
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::cell::UnsafeCell;
use core::net::Ipv4Addr;
use core::ops::{Deref, DerefMut};
use smoltcp::phy::DeviceCapabilities;
use smoltcp::wire::{EthernetAddress, HardwareAddress};
use smoltcp::{
    phy::{self},
    wire::{IpAddress, IpCidr},
};

/// 路由动作
#[derive(Debug, PartialEq)]
pub enum RoutingAction {
    DeliverToLocal, // 交给本地协议栈处理
    Forwarded,      // 已转发给其他接口
    Drop,           // 丢弃
    Ignore,         // 忽略
}

pub struct RouteRxToken {
    driver: RouteDriver,
    buffer: Vec<u8>,
}

impl phy::RxToken for RouteRxToken {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        //? 如果buffer是L3包（来自Router注入），直接交给协议栈
        if self.is_l3_packet() {
            return f(self.buffer.as_slice());
        }

        // 如果是完整的以太网帧，先让Router分析
        let routing_action = if let Some(router) = self.driver.router.upgrade() {
            router.handle_received_frame(&self.driver.name(), &self.buffer)
        } else {
            RoutingAction::DeliverToLocal
        };

        match routing_action {
            RoutingAction::DeliverToLocal => f(self.buffer.as_slice()),
            _ => f(&[]),
        }
    }
}

impl RouteRxToken {
    pub fn is_l3_packet(&self) -> bool {
        if self.buffer.len() < 20 {
            return false;
        }

        // 检查IPv4包特征
        let first_byte = self.buffer[0];
        let version = (first_byte >> 4) & 0x0F;
        let ihl = (first_byte & 0x0F) as usize * 4;

        if version != 4 || ihl < 20 || self.buffer.len() < ihl {
            return false;
        }

        if self.buffer.len() >= 4 {
            let total_length = u16::from_be_bytes([self.buffer[2], self.buffer[3]]) as usize;
            return self.buffer.len() == total_length;
        }

        false
    }
}

pub struct RouteTxToken {
    driver: RouteDriver,
}

impl phy::TxToken for RouteTxToken {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let mut buffer = vec![0; len];
        let result = f(buffer.as_mut_slice());
        let mut device = self.driver.inner.lock();
        device.route_transmit(buffer);
        result
    }
}

pub struct Route {
    name: String,
    rx_queue: VecDeque<Vec<u8>>,
    tx_queue: VecDeque<Vec<u8>>,
    l3_inject_queue: VecDeque<Vec<u8>>,
}

impl Route {
    pub fn new(name: &str) -> Self {
        Route {
            name: name.to_string(),
            rx_queue: VecDeque::new(),
            tx_queue: VecDeque::new(),
            l3_inject_queue: VecDeque::new(),
        }
    }

    pub fn inject_ether(&mut self, data: Vec<u8>) {
        self.rx_queue.push_back(data);
    }

    pub fn route_receive(&mut self) -> Vec<u8> {
        // 优先处理L3注入的包
        if let Some(l3_packet) = self.l3_inject_queue.pop_front() {
            return l3_packet;
        }

        // 然后处理硬件接收的帧
        self.rx_queue.pop_front().unwrap_or_else(|| Vec::new())
    }

    pub fn route_transmit(&mut self, buffer: Vec<u8>) {
        self.tx_queue.push_back(buffer);
    }

    pub fn pop_tx_frame(&mut self) -> Option<Vec<u8>> {
        self.tx_queue.pop_front()
    }

    pub fn inject_l3_packet(&mut self, ip_packet: Vec<u8>) {
        self.l3_inject_queue.push_back(ip_packet);
    }
}

#[derive(Debug)]
struct RouteDriverWapper(UnsafeCell<RouteDriver>);
unsafe impl Send for RouteDriverWapper {}
unsafe impl Sync for RouteDriverWapper {}

impl Deref for RouteDriverWapper {
    type Target = RouteDriver;
    fn deref(&self) -> &Self::Target {
        unsafe { &*self.0.get() }
    }
}
impl DerefMut for RouteDriverWapper {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { &mut *self.0.get() }
    }
}

impl RouteDriverWapper {
    #[allow(clippy::mut_from_ref)]
    #[allow(clippy::mut_from_ref)]
    fn force_get_mut(&self) -> &mut RouteDriver {
        unsafe { &mut *self.0.get() }
    }
}

pub struct RouteDriver {
    pub inner: Arc<SpinLock<Route>>,
    pub router: Weak<Router>,
}

impl RouteDriver {
    pub fn new(name: &str) -> Self {
        let inner = Arc::new(SpinLock::new(Route::new(name)));
        RouteDriver {
            inner,
            router: Weak::default(),
        }
    }

    pub fn name(&self) -> String {
        self.inner.lock().name.clone()
    }

    pub fn attach_router(&mut self, router: Arc<Router>) {
        self.router = Arc::downgrade(&router);
    }
}

impl Clone for RouteDriver {
    fn clone(&self) -> Self {
        RouteDriver {
            inner: self.inner.clone(),
            router: self.router.clone(),
        }
    }
}

impl phy::Device for RouteDriver {
    type RxToken<'a>
        = RouteRxToken
    where
        Self: 'a;
    type TxToken<'a>
        = RouteTxToken
    where
        Self: 'a;

    fn capabilities(&self) -> phy::DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.max_transmission_unit = 1500;
        caps.medium = smoltcp::phy::Medium::Ethernet;
        caps
    }

    fn receive(
        &mut self,
        _timestamp: smoltcp::time::Instant,
    ) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        let buffer = self.inner.lock().route_receive();

        // if let Some(router) = self.router.upgrade() {
        //     router.recv_from_iface(buffer);
        //     return None;
        // }

        if buffer.is_empty() {
            return None;
        }
        let rx = RouteRxToken {
            driver: self.clone(),
            buffer,
        };
        let tx = RouteTxToken {
            driver: self.clone(),
        };
        Some((rx, tx))
    }

    fn transmit(&mut self, _timestamp: smoltcp::time::Instant) -> Option<Self::TxToken<'_>> {
        Some(RouteTxToken {
            driver: self.clone(),
        })
    }
}

#[cast_to([sync] Iface)]
#[cast_to([sync] crate::driver::base::device::Device)]
#[derive(Debug)]
pub struct RouteInterface {
    name: String,
    driver: RouteDriverWapper,
    common: IfaceCommon,
    inner: SpinLock<InnerRouteInterface>,
    locked_kobj_state: LockedKObjectState,
}

#[derive(Debug)]
pub struct InnerRouteInterface {
    netdevice_common: NetDeviceCommonData,
    device_common: DeviceCommonData,
    kobj_common: KObjectCommonData,

    router: Weak<Router>,
    // 本接口的子网信息
    subnet: Option<IpCidr>,
}

impl Default for InnerRouteInterface {
    fn default() -> Self {
        InnerRouteInterface {
            netdevice_common: NetDeviceCommonData::default(),
            device_common: DeviceCommonData::default(),
            kobj_common: KObjectCommonData::default(),
            router: Weak::default(),
            subnet: None,
        }
    }
}

impl RouteInterface {
    pub fn new(driver: RouteDriver) -> Arc<Self> {
        let iface_id = generate_iface_id();

        let mac = [
            0x03,
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

        Arc::new(RouteInterface {
            name: driver.name(),
            driver: RouteDriverWapper(UnsafeCell::new(driver)),
            common: IfaceCommon::new(iface_id, false, iface),
            inner: SpinLock::new(InnerRouteInterface::default()),
            locked_kobj_state: LockedKObjectState::default(),
        })
    }

    // fn send_via_raw_socket(&self, ip_packet: Vec<u8>, dst_ip: Ipv4Addr) {
    //     let raw_rx_buffer =
    //         RawSocketBuffer::new(vec![raw::PacketMetadata::EMPTY; 4], vec![0; 1024]);
    //     let raw_tx_buffer =
    //         RawSocketBuffer::new(vec![raw::PacketMetadata::EMPTY; 4], vec![0; 1024]);
    //     let raw_socket = raw::Socket::new(
    //         smoltcp::wire::IpVersion::Ipv4,
    //         smoltcp::wire::IpProtocol::Unknown(0), // 接受所有协议
    //         raw_rx_buffer,
    //         raw_tx_buffer,
    //     );
    // }

    pub fn inject_l3_packet_for_sending(&self, ip_packet: Vec<u8>) {
        let mut device = self.driver.inner.lock();
        device.inject_l3_packet(ip_packet);
    }

    pub fn receive_frame_from_hardware(&self, frame: Vec<u8>) {
        let mut device = self.driver.inner.lock();
        device.inject_ether(frame);
    }

    pub fn get_outgoing_frame(&self) -> Option<Vec<u8>> {
        let mut device = self.driver.inner.lock();
        device.pop_tx_frame()
    }

    pub fn set_subnet(&self, cidr: IpCidr) {
        let mut inner = self.inner();
        inner.subnet = Some(cidr);
    }

    pub fn subnet(&self) -> Option<IpCidr> {
        self.inner().subnet
    }

    pub fn is_in_subnet(&self, ip: Ipv4Addr) -> bool {
        if let Some(subnet) = self.subnet() {
            subnet.contains_addr(&IpAddress::Ipv4(ip))
        } else {
            false
        }
    }

    pub fn attach_router(&self, router: Arc<Router>) {
        self.inner().router = Arc::downgrade(&router);
        self.driver.force_get_mut().attach_router(router);
    }

    pub fn update_ip_addrs(&self, cidr: IpCidr) {
        let iface = &mut self.common.smol_iface.lock_irqsave();
        iface.update_ip_addrs(|ip_addrs| {
            ip_addrs.push(cidr).expect("Push ipCidr failed: full");
        });
    }

    pub fn add_default_route(&self, ip: IpAddress) {
        let iface = &mut self.common.smol_iface.lock_irqsave();

        iface.routes_mut().update(|routes_map| {
            routes_map
                .push(smoltcp::iface::Route {
                    cidr: IpCidr::new(IpAddress::v4(0, 0, 0, 0), 0),
                    via_router: ip,
                    preferred_until: None,
                    expires_at: None,
                })
                .expect("Add default route to peer failed");
        });
    }

    fn inner(&self) -> SpinLockGuard<InnerRouteInterface> {
        return self.inner.lock();
    }

    pub fn is_self_ip(&self, dst_ip: Ipv4Addr) -> bool {
        let iface = self.common.smol_iface.lock();
        iface.ip_addrs().iter().any(|cidr| {
            if let IpAddress::Ipv4(ip) = cidr.address() {
                ip == dst_ip
            } else {
                false
            }
        })
    }

    // pub fn send()!!!
}

impl KObject for RouteInterface {
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

    fn set_name(&self, _name: String) {
        // do nothing
    }

    fn kobj_state(&self) -> RwLockReadGuard<KObjectState> {
        self.locked_kobj_state.read()
    }

    fn kobj_state_mut(&self) -> RwLockWriteGuard<KObjectState> {
        self.locked_kobj_state.write()
    }

    fn set_kobj_state(&self, state: KObjectState) {
        *self.locked_kobj_state.write() = state;
    }

    fn set_kobj_type(&self, ktype: Option<&'static dyn KObjType>) {
        self.inner().kobj_common.kobj_type = ktype;
    }
}

impl Device for RouteInterface {
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

        return r;
    }

    fn set_class(&self, class: Option<Weak<dyn Class>>) {
        self.inner().device_common.class = class;
    }

    fn driver(&self) -> Option<Arc<dyn Driver>> {
        let r = self.inner().device_common.driver.clone()?.upgrade();
        if r.is_none() {
            self.inner().device_common.driver = None;
        }

        return r;
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

    fn dev_parent(&self) -> Option<Weak<dyn Device>> {
        self.inner().device_common.get_parent_weak_or_clear()
    }

    fn set_dev_parent(&self, parent: Option<Weak<dyn Device>>) {
        self.inner().device_common.parent = parent;
    }
}

impl Iface for RouteInterface {
    fn common(&self) -> &IfaceCommon {
        &self.common
    }

    fn iface_name(&self) -> String {
        self.name.clone()
    }

    fn mac(&self) -> smoltcp::wire::EthernetAddress {
        let mac = [0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
        smoltcp::wire::EthernetAddress(mac)
    }

    fn poll(&self) {
        self.common.poll(self.driver.force_get_mut())
    }

    fn addr_assign_type(&self) -> u8 {
        return self.inner().netdevice_common.addr_assign_type;
    }

    fn net_device_type(&self) -> u16 {
        return self.inner().netdevice_common.net_device_type;
    }

    fn net_state(&self) -> NetDeivceState {
        return self.inner().netdevice_common.state;
    }

    fn set_net_state(&self, state: NetDeivceState) {
        self.inner().netdevice_common.state |= state;
    }

    fn operstate(&self) -> Operstate {
        return self.inner().netdevice_common.operstate;
    }

    fn set_operstate(&self, state: Operstate) {
        self.inner().netdevice_common.operstate = state;
    }
}
