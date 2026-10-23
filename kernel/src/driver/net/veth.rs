use super::bridge::BridgeEnableDevice;
use super::PacketBuffer;
use super::{BootstrapRoute, Iface, IfaceCommon, IfacePollScope, MtuBounds, ETHERNET_MAX_IP_MTU};
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
use crate::driver::net::bridge::{BridgeCommonData, BridgePort, BridgePortId};
use crate::driver::net::napi::{napi_schedule, NapiStruct};
use crate::driver::net::register_netdevice;
use crate::driver::net::types::InterfaceFlags;
use crate::filesystem::kernfs::KernFSInode;
use crate::init::initcall::INITCALL_DEVICE;
use crate::libs::mutex::Mutex;
use crate::libs::prepared_queue::{Node, Queue};
use crate::libs::rwsem::{RwSemReadGuard, RwSemWriteGuard};
use crate::libs::spinlock::{SpinLock, SpinLockGuard};
use crate::net::generate_iface_id;
use crate::net::route::{RTN_UNICAST, RTPROT_BOOT, RT_SCOPE_UNIVERSE, RT_TABLE_MAIN};
use crate::net::routing::RouterEnableDevice;
use crate::process::namespace::net_namespace::{NetNamespace, INIT_NET_NAMESPACE};
use alloc::boxed::Box;
use alloc::fmt::Debug;
use alloc::string::{String, ToString};
use alloc::sync::{Arc, Weak};
use smoltcp::phy::DeviceCapabilities;
use smoltcp::phy::{self, RxToken};
use smoltcp::wire::{EthernetAddress, EthernetFrame, HardwareAddress, IpAddress, IpCidr};
use system_error::SystemError;
use unified_init::macros::unified_init;

const VETH_IP_MTU: usize = 1500;
const VETH_MAX_FRAME_SIZE: usize = ETHERNET_MAX_IP_MTU + 14;
const VETH_MAX_PENDING_FRAMES: usize = 1024;
const VETH_MAX_PENDING_BYTES: usize = 4 * 1024 * 1024;

pub struct Veth {
    name: String,
    /// Frames awaiting bridge/routing classification. Classification may
    /// consult the authoritative FIB and therefore must run outside smoltcp's
    /// interface lock.
    pending_rx_queue: Queue<PendingVethFrame>,
    /// Frames classified for local smoltcp delivery.
    local_rx_queue: Queue<PendingVethFrame>,
    queued_frames: usize,
    queued_bytes: usize,
    reserved_frames: usize,
    reserved_bytes: usize,
    /// Serialized with peer enqueue so DOWN both rejects new external ingress
    /// and drains everything accepted before the transition.
    peer_ingress_enabled: bool,
    ingress_generation: u64,
    /// 对端的 `VethInterface`，在完成数据发送的时候会使用到
    peer: Weak<VethInterface>,
    self_iface_ref: Weak<VethInterface>,
}

impl Veth {
    pub fn new(name: String) -> Self {
        Veth {
            name,
            pending_rx_queue: Queue::new(),
            local_rx_queue: Queue::new(),
            queued_frames: 0,
            queued_bytes: 0,
            reserved_frames: 0,
            reserved_bytes: 0,
            peer_ingress_enabled: true,
            ingress_generation: 0,
            peer: Weak::new(),
            self_iface_ref: Weak::new(),
        }
    }

    pub fn set_peer_iface(&mut self, peer: &Arc<VethInterface>) {
        self.peer = Arc::downgrade(peer);
    }

    fn to_bridge(bridge_data: &BridgeCommonData, data: &[u8]) {
        // log::info!("Veth {} sending data to bridge", self.name);
        let Some(bridge) = bridge_data.bridge_driver_ref.upgrade() else {
            log::warn!("Bridge has been dropped");
            return;
        };
        bridge.handle_frame(bridge_data.id, data);
    }

    fn recv_local(&mut self) -> Option<PacketBuffer> {
        let frame = self.local_rx_queue.pop_node()?.into_value();
        self.queued_frames -= 1;
        self.queued_bytes -= frame.data.capacity();
        Some(frame.data)
    }

    pub fn name(&self) -> &str {
        &self.name
    }
}

#[derive(Clone)]
pub struct VethDriver {
    pub inner: Arc<SpinLock<Veth>>,
    /// Serializes dequeue, classification, and local enqueue as one ordered
    /// ingress batch across NAPI, netns, and synchronous socket pollers.
    ingress_classification_lock: Arc<Mutex<()>>,
    /// A shared weak reference to the owning network interface, used for packet
    /// socket delivery.
    iface: Arc<SpinLock<Weak<dyn Iface>>>,
}

impl Debug for VethDriver {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("VethDriver")
            .field("name", &self.name())
            .finish()
    }
}

enum IngressDisposition {
    Consumed,
    Local,
}

struct PendingVethFrame {
    generation: u64,
    data: PacketBuffer,
}

impl VethDriver {
    /// Classifies a bounded batch before smoltcp locks its interface. Routed
    /// and bridged frames are consumed here; local frames move to the queue
    /// exposed through `Device::receive`.
    fn prepare_ingress(&self, scan_budget: usize) -> usize {
        let _classification_guard = self.ingress_classification_lock.lock();
        let mut scanned = 0;
        for _ in 0..scan_budget {
            let frame = {
                let mut veth = self.inner.lock();
                veth.pending_rx_queue.pop_node()
            };
            let Some(frame) = frame else {
                break;
            };
            scanned += 1;

            let local = matches!(
                self.classify_and_consume(&frame.value().data),
                IngressDisposition::Local
            );
            let mut inner = self.inner.lock();
            if local
                && inner.peer_ingress_enabled
                && inner.ingress_generation == frame.value().generation
            {
                // Keep the exact prepared node and its accepted byte budget.
                inner.local_rx_queue.push_node(frame);
            } else {
                inner.queued_frames -= 1;
                inner.queued_bytes -= frame.value().data.capacity();
                drop(inner);
                drop(frame);
            }
        }
        scanned
    }

    fn classify_and_consume(&self, data: &[u8]) -> IngressDisposition {
        let Some(iface) = self.inner.lock().self_iface_ref.upgrade() else {
            return IngressDisposition::Local;
        };
        // Like Linux's ptype_all taps, packet sockets observe ingress before
        // bridge/routing can consume it, not only frames delivered locally.
        let packet_iface: Arc<dyn Iface> = iface.clone();
        let pkt_type = crate::net::socket::packet::classify_packet(data, &packet_iface);
        crate::net::socket::packet::deliver_to_packet_sockets(&packet_iface, data, pkt_type);

        if let Some(bridge_data) = iface.common_bridge_data() {
            Veth::to_bridge(&bridge_data, data);
            return IngressDisposition::Consumed;
        }

        let Ok(frame) = EthernetFrame::new_checked(data) else {
            return IngressDisposition::Local;
        };
        // Non-bridge IP must pass through smoltcp's validated ingress path
        // before PRE_ROUTING and the FIB decision. The old pre-smoltcp
        // router would bypass nftables (and conntrack) for either family.
        if matches!(
            frame.ethertype(),
            smoltcp::wire::EthernetProtocol::Ipv4 | smoltcp::wire::EthernetProtocol::Ipv6
        ) {
            return IngressDisposition::Local;
        }
        match iface.handle_routable_packet(&frame) {
            Ok(()) => IngressDisposition::Consumed,
            Err(Some(error)) => {
                log::error!("Router error: {:?}", error);
                IngressDisposition::Consumed
            }
            Err(None) => IngressDisposition::Local,
        }
    }

    fn has_pending_ingress(&self) -> bool {
        !self.inner.lock().pending_rx_queue.is_empty()
    }

    fn has_local_ingress(&self) -> bool {
        !self.inner.lock().local_rx_queue.is_empty()
    }

    fn validate_frame_len(&self, frame: &[u8]) -> Result<(), SystemError> {
        let max_untagged = self.iface().map_or(VETH_IP_MTU, |iface| iface.mtu()) + 14;
        if frame.len() <= max_untagged {
            return Ok(());
        }
        let vlan_tagged = frame.len() >= 14
            && matches!(u16::from_be_bytes([frame[12], frame[13]]), 0x8100 | 0x88a8);
        if vlan_tagged && frame.len() <= max_untagged + 4 {
            Ok(())
        } else {
            Err(SystemError::EMSGSIZE)
        }
    }

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
            ingress_classification_lock: Arc::new(Mutex::new(())),
            iface: Arc::new(SpinLock::new(Weak::<VethInterface>::new())),
        };
        let driver2 = VethDriver {
            inner: dev2,
            ingress_classification_lock: Arc::new(Mutex::new(())),
            iface: Arc::new(SpinLock::new(Weak::<VethInterface>::new())),
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

    fn reserve_slot(&self, len: usize) -> Result<VethReservation, SystemError> {
        self.reserve_storage(len, len)
    }

    fn reserve_storage(
        &self,
        wire_len: usize,
        bytes: usize,
    ) -> Result<VethReservation, SystemError> {
        // The exact VLAN bytes are checked at commit. Bound unknown bytes by
        // the largest frame this device can accept before preparing storage.
        let mtu = self.iface().map_or(VETH_IP_MTU, |iface| iface.mtu());
        if wire_len > mtu + 18 {
            return Err(SystemError::EMSGSIZE);
        }
        let mut node = Node::try_new(PendingVethFrame {
            generation: 0,
            data: PacketBuffer::new(),
        })?;
        let peer = self
            .inner
            .lock()
            .peer
            .upgrade()
            .ok_or(SystemError::ENOBUFS)?;
        let napi = peer.napi_struct().ok_or(SystemError::ENOBUFS)?;
        let mut inner = peer.driver.inner.lock();
        if !inner.peer_ingress_enabled {
            return Err(SystemError::ENETDOWN);
        }
        if inner.queued_frames + inner.reserved_frames >= VETH_MAX_PENDING_FRAMES
            || bytes
                > VETH_MAX_PENDING_BYTES.saturating_sub(inner.queued_bytes + inner.reserved_bytes)
        {
            return Err(SystemError::ENOBUFS);
        }
        node.value_mut().generation = inner.ingress_generation;
        inner.reserved_frames += 1;
        inner.reserved_bytes += bytes;
        drop(inner);
        Ok(VethReservation {
            source: self.clone(),
            peer,
            napi,
            node: Some(node),
            bytes,
        })
    }

    fn submit_frame(&self, frame: PacketBuffer) -> Result<(), SystemError> {
        self.validate_frame_len(&frame)?;
        let mut prepared = self.reserve_storage(frame.len(), frame.capacity())?;
        prepared.node.as_mut().unwrap().value_mut().data = frame;
        prepared.commit()
    }

    pub fn try_raw_transmit(&self, frame: &[u8]) -> Result<(), SystemError> {
        self.validate_frame_len(frame)?;
        let mut prepared = self.reserve_slot(frame.len())?;
        prepared.node.as_mut().unwrap().value_mut().data = PacketBuffer::try_from_slice(frame)?;
        prepared.commit()
    }
}

struct VethReservation {
    source: VethDriver,
    peer: Arc<VethInterface>,
    napi: Arc<NapiStruct>,
    node: Option<Box<Node<PendingVethFrame>>>,
    bytes: usize,
}

impl VethReservation {
    fn commit(mut self) -> Result<(), SystemError> {
        self.source
            .validate_frame_len(&self.node.as_ref().unwrap().value().data)?;
        if let Some(iface) = self.source.iface() {
            crate::net::socket::packet::deliver_to_packet_sockets(
                &iface,
                &self.node.as_ref().unwrap().value().data,
                crate::net::socket::packet::PacketType::Outgoing,
            );
        }
        debug_assert_eq!(
            self.node.as_ref().unwrap().value().data.capacity(),
            self.bytes
        );
        let node = self.node.take().unwrap();
        let mut inner = self.peer.driver.inner.lock();
        inner.reserved_frames -= 1;
        inner.reserved_bytes -= self.bytes;
        if !inner.peer_ingress_enabled || inner.ingress_generation != node.value().generation {
            return Err(SystemError::ENETDOWN);
        }
        inner.queued_frames += 1;
        inner.queued_bytes += self.bytes;
        inner.pending_rx_queue.push_node(node);
        drop(inner);
        napi_schedule(self.napi.clone());
        Ok(())
    }
}

impl Drop for VethReservation {
    fn drop(&mut self) {
        if self.node.is_some() {
            let mut inner = self.peer.driver.inner.lock();
            inner.reserved_frames -= 1;
            inner.reserved_bytes -= self.bytes;
        }
    }
}

pub struct VethTxToken {
    driver: VethDriver,
    prepared: Option<VethReservation>,
}

impl phy::TxToken for VethTxToken {
    fn prepare_buffer(&mut self, len: usize) -> Result<(), phy::TxEgressError> {
        if self
            .prepared
            .as_ref()
            .is_some_and(|prepared| prepared.bytes == len)
        {
            return Ok(());
        }
        // Keep any earlier valid preparation until its replacement succeeds.
        let mut prepared = self
            .driver
            .reserve_slot(len)
            .map_err(|_| phy::TxEgressError::Exhausted)?;
        let mut buffer =
            PacketBuffer::try_with_capacity(len).map_err(|_| phy::TxEgressError::Exhausted)?;
        buffer
            .resize(len, 0)
            .map_err(|_| phy::TxEgressError::Exhausted)?;
        prepared.node.as_mut().unwrap().value_mut().data = buffer;
        self.prepared = Some(prepared);
        Ok(())
    }
    fn consume<R, F>(mut self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let mut prepared = self
            .prepared
            .take()
            .expect("veth TX requires exact preparation");
        assert_eq!(prepared.bytes, len);
        let result = f(prepared
            .node
            .as_mut()
            .unwrap()
            .value_mut()
            .data
            .as_mut_slice());
        let _ = prepared.commit();
        result
    }
}

pub struct VethRxToken {
    buffer: PacketBuffer,
}

impl RxToken for VethRxToken {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        f(self.buffer.as_slice())
    }
}

impl phy::Device for VethDriver {
    type RxToken<'a> = VethRxToken;
    type TxToken<'a> = VethTxToken;

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        // This is the stack's reconfiguration ceiling, not its current MTU.
        caps.max_transmission_unit = VETH_MAX_FRAME_SIZE;
        caps.medium = smoltcp::phy::Medium::Ethernet;
        caps
    }

    fn receive(
        &mut self,
        _timestamp: smoltcp::time::Instant,
    ) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        let mut guard = self.inner.lock();
        guard.recv_local().map(|buf| {
            // log::info!("VethDriver received data: {:?}", buf);
            (
                VethRxToken { buffer: buf },
                VethTxToken {
                    driver: self.clone(),
                    prepared: None,
                },
            )
        })
    }

    fn transmit(&mut self, _timestamp: smoltcp::time::Instant) -> Option<Self::TxToken<'_>> {
        Some(VethTxToken {
            driver: self.clone(),
            prepared: None,
        })
    }
}

#[cast_to([sync] Iface)]
#[cast_to([sync] device::Device)]
#[derive(Debug)]
pub struct VethInterface {
    driver: VethDriver,
    common: IfaceCommon,
    mac_address: SpinLock<EthernetAddress>,
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
    /// A netns move must not carry protocol reassembly, fragments, neighbors,
    /// or multicast state across the namespace boundary. Prepare a fresh
    /// smoltcp interface before the move's irreversible sysfs transaction.
    pub(crate) fn prepare_stack_for_netns_move(
        &self,
    ) -> Result<smoltcp::iface::Interface, SystemError> {
        let mut config = smoltcp::iface::Config::new(HardwareAddress::Ethernet(self.mac()));
        config.random_seed = rand() as u64;
        let mut stack = smoltcp::iface::Interface::new(
            config,
            &mut self.driver.clone(),
            crate::time::Instant::now().into(),
        );
        stack.set_any_ip(true);
        stack.set_route_table_includes_connected_prefixes(true);
        stack
            .set_ip_mtu(self.stack_mtu(self.mtu()))
            .map_err(|_| SystemError::EINVAL)?;
        Ok(stack)
    }

    pub(crate) fn publish_stack_after_netns_move(&self, stack: smoltcp::iface::Interface) {
        // Memberships belonged to the source namespace. Keep the established
        // refcount -> smoltcp lock order while replacing both pieces of state;
        // a target namespace must join its groups on the fresh stack itself.
        let mut groups = self.common.multicast_refcnt.lock();
        *self.smol_iface().lock() = stack;
        groups.clear();
    }

    pub fn peer_veth_opt(&self) -> Option<Arc<VethInterface>> {
        self.inner.lock().peer_veth.upgrade()
    }

    pub fn new(driver: VethDriver) -> Arc<Self> {
        Self::new_with_initial_state(driver, true)
    }

    fn new_with_initial_state(driver: VethDriver, initially_up: bool) -> Arc<Self> {
        let iface_id = generate_iface_id();
        let mac = [
            0x02,
            (iface_id >> 32) as u8,
            (iface_id >> 24) as u8,
            (iface_id >> 16) as u8,
            (iface_id >> 8) as u8,
            iface_id as u8,
        ];
        let mac_address = EthernetAddress(mac);
        let hw_addr = HardwareAddress::Ethernet(mac_address);
        let mut iface_config = smoltcp::iface::Config::new(hw_addr);
        iface_config.random_seed = rand() as u64;
        let mut iface = smoltcp::iface::Interface::new(
            iface_config,
            &mut driver.clone(),
            crate::time::Instant::now().into(),
        );
        iface.set_any_ip(true);

        let mut flags = InterfaceFlags::BROADCAST | InterfaceFlags::MULTICAST;
        if initially_up {
            flags |= InterfaceFlags::UP | InterfaceFlags::RUNNING | InterfaceFlags::LOWER_UP;
        }
        let mtu = VETH_IP_MTU;

        let device = Arc::new(VethInterface {
            driver: driver.clone(),
            common: IfaceCommon::new(
                iface_id,
                super::types::InterfaceType::ETHER,
                driver.name(),
                mtu,
                flags,
                iface,
            ),
            mac_address: SpinLock::new(mac_address),
            inner: SpinLock::new(VethCommonData::default()),
            locked_kobj_state: LockedKObjectState::default(),
        });
        if !initially_up {
            device.set_net_state(NetDeivceState::__LINK_STATE_NOCARRIER);
            device.set_operstate(Operstate::IF_OPER_DOWN);
            device.driver.inner.lock().peer_ingress_enabled = false;
        }
        let napi_struct = NapiStruct::new(device.clone(), 10);
        *device.common.napi_struct.write() = Some(napi_struct);

        // 设置 driver 对接口的弱引用，用于 packet socket 分发
        device
            .driver
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
        Self::new_pair_with_initial_state(name1, name2, true)
    }

    /// Runtime-created veth links start administratively down, like Linux.
    pub fn new_pair_dynamic(name1: &str, name2: &str) -> (Arc<Self>, Arc<Self>) {
        Self::new_pair_with_initial_state(name1, name2, false)
    }

    fn new_pair_with_initial_state(
        name1: &str,
        name2: &str,
        initially_up: bool,
    ) -> (Arc<Self>, Arc<Self>) {
        let (driver1, driver2) = VethDriver::new_pair(name1, name2);
        let iface1 = VethInterface::new_with_initial_state(driver1, initially_up);
        let iface2 = VethInterface::new_with_initial_state(driver2, initially_up);

        iface1.set_peer_iface(&iface2);
        iface2.set_peer_iface(&iface1);

        (iface1, iface2)
    }

    pub fn bridge_master(&self) -> Option<Arc<super::bridge::BridgeIface>> {
        self.common_bridge_data()?
            .bridge_driver_ref
            .upgrade()?
            .local_iface()
    }

    fn update_bridge_carrier(&self) {
        if let Some(data) = self.common_bridge_data() {
            if let Some(bridge) = data.bridge_driver_ref.upgrade() {
                bridge.update_carrier_from_ports();
            }
        }
    }

    pub fn set_mac(&self, mac: EthernetAddress) -> Result<(), SystemError> {
        let bytes = mac.as_bytes();
        if bytes[0] & 1 != 0 || bytes.iter().all(|byte| *byte == 0) {
            return Err(SystemError::EINVAL);
        }
        let napi = self.napi_struct();
        if let Some(napi) = napi.as_ref() {
            super::napi::napi_pause_and_wait(napi);
        }
        self.common
            .smol_iface
            .lock()
            .set_hardware_addr(HardwareAddress::Ethernet(mac));
        let old = core::mem::replace(&mut *self.mac_address.lock(), mac);
        if let Some(data) = self.common_bridge_data() {
            if let Some(bridge) = data.bridge_driver_ref.upgrade() {
                bridge.port_mac_changed(data.id, old, mac);
            }
        }
        if let Some(napi) = napi {
            super::napi::napi_resume(napi);
        }
        Ok(())
    }

    fn update_carrier_with_peer(&self, self_up: bool) {
        let peer = self.peer_veth_opt();
        let peer_up = peer.as_ref().is_some_and(|other| {
            other.flags().contains(InterfaceFlags::UP)
                && other
                    .net_state()
                    .contains(NetDeivceState::__LINK_STATE_START)
        });
        let carrier = self_up && peer_up;
        for endpoint in core::iter::once(self).chain(peer.as_deref()) {
            if carrier {
                endpoint.clear_net_state(NetDeivceState::__LINK_STATE_NOCARRIER);
                endpoint.set_operstate(Operstate::IF_OPER_UP);
            } else {
                endpoint.set_net_state(NetDeivceState::__LINK_STATE_NOCARRIER);
                endpoint.set_operstate(Operstate::IF_OPER_DOWN);
            }
        }
    }

    /// Stop both endpoints before either can be unpublished. The caller keeps
    /// ownership of both Arc references and unregisters them after this returns.
    pub fn shutdown_pair(&self) {
        let peer = self.peer_veth_opt();
        self.common.close_tx_and_wait();
        self.begin_admin_down();
        if let Some(other) = peer.as_ref() {
            other.common.close_tx_and_wait();
            other.begin_admin_down();
        }
        for endpoint in core::iter::once(self).chain(peer.as_deref()) {
            if let Some(napi) = endpoint.napi_struct() {
                super::napi::napi_pause_and_wait(&napi);
                endpoint.quiesce_admin_down();
                super::napi::napi_disable(&napi);
            } else {
                endpoint.quiesce_admin_down();
            }
            endpoint.inner.lock().peer_veth = Weak::new();
            endpoint.driver.inner.lock().peer = Weak::new();
            endpoint.set_net_state(NetDeivceState::__LINK_STATE_NOCARRIER);
            endpoint.set_operstate(Operstate::IF_OPER_DOWN);
        }
    }

    fn inner(&self) -> SpinLockGuard<'_, VethCommonData> {
        self.inner.lock()
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
        self.common.name()
    }

    fn set_name(&self, name: String) {
        self.common.set_name(name);
    }
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
        IdTable::new(self.common.name(), None)
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
    fn destroy_on_netns_exit(&self) -> bool {
        true
    }

    fn common(&self) -> &IfaceCommon {
        &self.common
    }

    fn iface_name(&self) -> String {
        self.common.name()
    }

    fn mac(&self) -> EthernetAddress {
        *self.mac_address.lock()
    }

    fn poll(&self) -> bool {
        let mut driver = self.driver.clone();
        match self.common.poll_scope() {
            IfacePollScope::None => false,
            IfacePollScope::LocalOnly => self.common.poll(&mut driver),
            IfacePollScope::Full => {
                let result = self.poll_napi(64);
                result.work_done != 0 || result.poll_again
            }
        }
    }

    fn poll_napi(&self, budget: usize) -> super::napi::NapiPollResult {
        let mut driver = self.driver.clone();
        match self.common.poll_scope() {
            IfacePollScope::None => return super::napi::NapiPollResult::idle(),
            IfacePollScope::LocalOnly => return self.common.poll_napi(&mut driver, budget),
            IfacePollScope::Full => {}
        }
        // When both queues are backlogged, reserve bounded progress for each
        // side. A single active queue may still use the full NAPI budget.
        let local_backlogged = driver.has_local_ingress() || self.common().has_local_input();
        let both_backlogged = local_backlogged && driver.has_pending_ingress();
        let local_budget = if both_backlogged {
            budget.div_ceil(2)
        } else {
            budget
        };
        let smoltcp = self
            .common
            .poll_napi_routed_ingress(&mut driver, local_budget);
        // Classification is real receive work even when a frame is moved to
        // the local smoltcp queue for a later pass. Charge every scanned frame
        // to this NAPI instance so the global packet budget remains fair.
        let prepared = driver.prepare_ingress(budget - smoltcp.work_done);
        super::napi::NapiPollResult::new(
            prepared + smoltcp.work_done,
            smoltcp.poll_again || driver.has_pending_ingress() || driver.has_local_ingress(),
        )
    }

    fn raw_transmit(&self, frame: &[u8]) -> Result<(), SystemError> {
        self.driver.try_raw_transmit(frame)
    }

    fn raw_transmit_owned(&self, frame: PacketBuffer) -> Result<(), SystemError> {
        self.driver.submit_frame(frame)
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
        if state
            .intersects(NetDeivceState::__LINK_STATE_START | NetDeivceState::__LINK_STATE_NOCARRIER)
        {
            self.update_bridge_carrier();
        }
    }

    fn clear_net_state(&self, state: NetDeivceState) {
        self.inner().netdevice_common.state &= !state;
        if state
            .intersects(NetDeivceState::__LINK_STATE_START | NetDeivceState::__LINK_STATE_NOCARRIER)
        {
            self.update_bridge_carrier();
        }
    }

    fn operstate(&self) -> Operstate {
        self.inner().netdevice_common.operstate
    }

    fn set_operstate(&self, state: Operstate) {
        self.inner().netdevice_common.operstate = state;
    }

    fn mtu(&self) -> usize {
        self.common.mtu()
    }

    fn mtu_bounds(&self) -> MtuBounds {
        MtuBounds {
            min: 68,
            max: ETHERNET_MAX_IP_MTU,
        }
    }

    fn begin_admin_down(&self) {
        let mut driver = self.driver.inner.lock();
        driver.peer_ingress_enabled = false;
        driver.ingress_generation = driver.ingress_generation.wrapping_add(1);
    }

    fn quiesce_admin_down(&self) {
        // Serialize with the complete dequeue/classify/commit section. Once
        // this returns, no frame accepted in the old generation can still
        // produce bridge, routing, packet-socket, or local-stack side effects.
        let classification_guard = self.driver.ingress_classification_lock.lock();
        let mut driver = self.driver.inner.lock();
        let pending = core::mem::take(&mut driver.pending_rx_queue);
        let local = core::mem::take(&mut driver.local_rx_queue);
        driver.queued_frames = 0;
        driver.queued_bytes = 0;
        // Existing unpublished reservations release their own counters, even
        // across DOWN/UP. Generation rejects their stale commit.
        drop(driver);
        drop(classification_guard);
        drop(pending);
        drop(local);
    }

    fn publish_admin_state(&self, is_up: bool) {
        if is_up {
            self.driver.inner.lock().peer_ingress_enabled = true;
        }
        self.update_carrier_with_peer(is_up);
    }
}

impl BridgeEnableDevice for VethInterface {
    fn receive_from_bridge(&self, frame: &[u8]) {
        if self.common_bridge_data().is_some() {
            if let Some(_tx) = self.common.try_acquire_tx() {
                let _ = self.driver.try_raw_transmit(frame);
            }
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

    fn clear_common_bridge_data(&self, bridge_id: BridgePortId) {
        let mut inner = self.inner.lock();
        if inner
            .bridge_common_data
            .as_ref()
            .is_some_and(|data| data.id == bridge_id)
        {
            inner.bridge_common_data = None;
        }
    }
}

impl RouterEnableDevice for VethInterface {}

fn veth_route_test() {
    let (iface_ns1, iface_host1) = VethInterface::new_pair("veth-ns1", "veth-host1");
    let (iface_ns2, iface_host2) = VethInterface::new_pair("veth-ns2", "veth-host2");

    let addr1 = IpAddress::v4(192, 168, 1, 1);
    let cidr1 = IpCidr::new(addr1, 24);
    crate::net::address::initialize_address(&(iface_ns1.clone() as Arc<dyn Iface>), cidr1)
        .expect("initialize veth address");

    let addr2 = IpAddress::v4(192, 168, 1, 254);
    let cidr2 = IpCidr::new(addr2, 24);
    crate::net::address::initialize_address(&(iface_host1.clone() as Arc<dyn Iface>), cidr2)
        .expect("initialize veth address");

    let addr3 = IpAddress::v4(192, 168, 2, 254);
    let cidr3 = IpCidr::new(addr3, 24);
    crate::net::address::initialize_address(&(iface_host2.clone() as Arc<dyn Iface>), cidr3)
        .expect("initialize veth address");

    let addr4 = IpAddress::v4(192, 168, 2, 3);
    let cidr4 = IpCidr::new(addr4, 24);
    crate::net::address::initialize_address(&(iface_ns2.clone() as Arc<dyn Iface>), cidr4)
        .expect("initialize veth address");

    // The fixture lives in one real netns. Endpoints use explicit host routes
    // to enter through their peers; ingress then exercises namespace-local
    // handoff to the interface that owns the destination address.
    for (iface, destination, gateway, scope) in [
        (&iface_ns1, addr4, Some(addr2), RT_SCOPE_UNIVERSE),
        (&iface_ns2, addr1, Some(addr3), RT_SCOPE_UNIVERSE),
        (&iface_host1, addr1, None, crate::net::route::RT_SCOPE_LINK),
        (&iface_host2, addr4, None, crate::net::route::RT_SCOPE_LINK),
    ] {
        let oif = u32::try_from(iface.nic_id()).expect("fixture ifindex overflow");
        iface
            .common
            .stage_bootstrap_route(BootstrapRoute {
                destination: IpCidr::new(destination, 32),
                source: None,
                preferred_source: None,
                table: RT_TABLE_MAIN,
                priority: 0,
                tos: 0,
                protocol: RTPROT_BOOT,
                scope,
                kind: RTN_UNICAST,
                oif,
                gateway,
                nexthop_flags: 0,
            })
            .expect("stage veth fixture host route");
    }

    let turn_on = |a: &Arc<VethInterface>, ns: Arc<NetNamespace>| {
        a.set_net_state(NetDeivceState::__LINK_STATE_START);
        a.set_operstate(Operstate::IF_OPER_UP);
        // NET_DEVICES.write_irqsave().insert(a.nic_id(), a.clone());
        register_netdevice(&ns, a.clone()).expect("register veth fixture interface");
    };

    turn_on(&iface_ns1, INIT_NET_NAMESPACE.clone());
    turn_on(&iface_ns2, INIT_NET_NAMESPACE.clone());
    turn_on(&iface_host1, INIT_NET_NAMESPACE.clone());
    turn_on(&iface_host2, INIT_NET_NAMESPACE.clone());
}

fn veth_epoll_test() {
    let (iface1, iface2) = VethInterface::new_pair("veth1", "veth2");

    let addr1 = IpAddress::v4(111, 111, 11, 1);
    let cidr1 = IpCidr::new(addr1, 24);
    crate::net::address::initialize_address(&(iface1.clone() as Arc<dyn Iface>), cidr1)
        .expect("initialize veth address");

    let addr2 = IpAddress::v4(111, 111, 11, 2);
    let cidr2 = IpCidr::new(addr2, 24);
    crate::net::address::initialize_address(&(iface2.clone() as Arc<dyn Iface>), cidr2)
        .expect("initialize veth address");

    let turn_on = |a: &Arc<VethInterface>, ns: Arc<NetNamespace>| {
        a.set_net_state(NetDeivceState::__LINK_STATE_START);
        a.set_operstate(Operstate::IF_OPER_UP);
        // NET_DEVICES.write_irqsave().insert(a.nic_id(), a.clone());
        register_netdevice(&ns, a.clone()).expect("register veth fixture interface");
    };

    turn_on(&iface1, INIT_NET_NAMESPACE.clone());
    turn_on(&iface2, INIT_NET_NAMESPACE.clone());
}

#[unified_init(INITCALL_DEVICE)]
pub fn veth_init() -> Result<(), SystemError> {
    if super::net_test_fixtures_enabled() {
        veth_epoll_test();
        veth_route_test();
        log::info!("Veth test fixtures initialized.");
    }
    Ok(())
}

#[cfg(feature = "mm_selftest")]
pub(crate) fn buffer_selftest() -> bool {
    fn run() -> Result<bool, SystemError> {
        // Private, unregistered interfaces exercise production admission and
        // generation rollback without scheduling synthetic packets globally.
        let (source, peer) = VethInterface::new_pair("mm262-source", "mm262-peer");
        *source.common.napi_struct.write() = Some(NapiStruct::new_disabled(source.clone(), 10));
        *peer.common.napi_struct.write() = Some(NapiStruct::new_disabled(peer.clone(), 10));
        let mut driver = source.driver.clone();
        let mut tx = <VethDriver as phy::Device>::transmit(
            &mut driver,
            smoltcp::time::Instant::from_millis(0),
        )
        .unwrap();
        phy::TxToken::prepare_buffer(&mut tx, 64).map_err(|_| SystemError::ENOMEM)?;
        phy::TxToken::prepare_buffer(&mut tx, 64).map_err(|_| SystemError::ENOMEM)?;
        if phy::TxToken::prepare_buffer(&mut tx, usize::MAX).is_ok() {
            return Ok(false);
        }
        {
            let inner = peer.driver.inner.lock();
            if inner.reserved_frames != 1 || inner.reserved_bytes != 64 {
                return Ok(false);
            }
        }
        phy::TxToken::consume(tx, 64, |frame| {
            frame.fill(0xa5);
            frame[12..14].copy_from_slice(&0x0800u16.to_be_bytes());
        });
        {
            let inner = peer.driver.inner.lock();
            if inner.queued_frames != 1
                || inner.queued_bytes != 64
                || inner.reserved_frames != 0
                || inner.reserved_bytes != 0
            {
                return Ok(false);
            }
        }
        if peer.driver.prepare_ingress(1) != 1 {
            return Ok(false);
        }
        let frame = peer
            .driver
            .inner
            .lock()
            .recv_local()
            .ok_or(SystemError::EIO)?;
        if frame.len() != 64
            || frame[..12].iter().any(|byte| *byte != 0xa5)
            || frame[12..14] != 0x0800u16.to_be_bytes()
            || frame[14..].iter().any(|byte| *byte != 0xa5)
        {
            return Ok(false);
        }
        drop(frame);
        if !super::local_output::native_output_selftest(&mut driver, true)?
            || peer.driver.prepare_ingress(1) != 1
        {
            return Ok(false);
        }
        let native = peer
            .driver
            .inner
            .lock()
            .recv_local()
            .ok_or(SystemError::EIO)?;
        if native.len() != 64 || native[14..].iter().any(|byte| *byte != 0xa5) {
            return Ok(false);
        }
        drop(native);
        let mut reservations = Queue::new();
        for _ in 0..VETH_MAX_PENDING_FRAMES {
            reservations.push_node(Node::try_new(driver.reserve_slot(1)?)?);
        }
        if !matches!(driver.reserve_slot(1), Err(SystemError::ENOBUFS)) {
            return Ok(false);
        }
        if !super::local_output::native_output_selftest(&mut driver, false)? {
            return Ok(false);
        }
        drop(reservations.pop_node());
        drop(driver.reserve_slot(1)?);
        drop(reservations);

        source.common.set_mtu(ETHERNET_MAX_IP_MTU);
        let bytes = VETH_MAX_FRAME_SIZE;
        let count = VETH_MAX_PENDING_BYTES / bytes;
        let mut reservations = Queue::new();
        for _ in 0..count {
            reservations.push_node(Node::try_new(driver.reserve_slot(bytes)?)?);
        }
        {
            let inner = peer.driver.inner.lock();
            if inner.reserved_frames != count
                || inner.reserved_bytes != count * bytes
                || count >= VETH_MAX_PENDING_FRAMES
            {
                return Ok(false);
            }
        }
        if !matches!(driver.reserve_slot(bytes), Err(SystemError::ENOBUFS)) {
            return Ok(false);
        }
        drop(reservations);
        source.common.set_mtu(VETH_IP_MTU);

        let mut old = driver.reserve_slot(64)?;
        let mut frame = PacketBuffer::try_with_capacity(64)?;
        frame.resize(64, 0)?;
        old.node.as_mut().unwrap().value_mut().data = frame;
        peer.begin_admin_down();
        peer.quiesce_admin_down();
        peer.publish_admin_state(true);
        if !matches!(old.commit(), Err(SystemError::ENETDOWN)) {
            return Ok(false);
        }
        let inner = peer.driver.inner.lock();
        Ok(inner.pending_rx_queue.is_empty()
            && inner.local_rx_queue.is_empty()
            && inner.queued_frames == 0
            && inner.queued_bytes == 0
            && inner.reserved_frames == 0
            && inner.reserved_bytes == 0)
    }
    run().unwrap_or(false)
}
