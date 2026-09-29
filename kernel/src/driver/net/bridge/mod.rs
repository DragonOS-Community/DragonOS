use crate::{
    arch::rand::rand,
    driver::{
        base::{
            class::Class,
            device::{self, bus::Bus, driver::Driver, DeviceCommonData, DeviceType, IdTable},
            kobject::{KObjType, KObject, KObjectCommonData, KObjectState, LockedKObjectState},
            kset::KSet,
        },
        net::{
            napi::{napi_schedule, NapiStruct},
            register_netdevice,
            types::{InterfaceFlags, InterfaceType},
            veth::VethInterface,
            Iface, IfaceCommon, IfacePollScope, MtuBounds, NetDeivceState, NetDeviceCommonData,
            Operstate,
        },
    },
    filesystem::kernfs::KernFSInode,
    init::initcall::INITCALL_DEVICE,
    libs::{
        rwlock::RwLock,
        rwsem::{RwSemReadGuard, RwSemWriteGuard},
        spinlock::{SpinLock, SpinLockGuard},
    },
    net::generate_iface_id,
    process::namespace::net_namespace::{NetNamespace, INIT_NET_NAMESPACE},
    time::Instant,
};
use alloc::string::ToString;
use alloc::sync::Weak;
use alloc::{collections::VecDeque, string::String, sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use hashbrown::HashMap;
use smoltcp::{
    phy::{self, DeviceCapabilities, RxToken},
    wire::{EthernetAddress, EthernetFrame, HardwareAddress, IpAddress, IpCidr},
};
use system_error::SystemError;
use unified_init::macros::unified_init;

mod forwarding;
mod iface;

pub use self::forwarding::{Bridge, BridgePort, BridgePortId};
use self::forwarding::{BridgeEgress, BRIDGE_MAX_LOCAL_FRAMES, BRIDGE_MTU};
pub use self::iface::BridgeIface;

#[derive(Debug)]
pub struct BridgeDriver {
    pub inner: SpinLock<Bridge>,
    pub netns: RwLock<Weak<NetNamespace>>,
    local_iface: SpinLock<Weak<BridgeIface>>,
    local_rx_queue: SpinLock<VecDeque<Vec<u8>>>,
    active: AtomicBool,
    mtu_set_by_user: AtomicBool,
    in_flight: AtomicUsize,
    self_ref: Weak<BridgeDriver>,
    next_port_id: AtomicUsize,
}

impl BridgeDriver {
    pub fn new(name: &str) -> Arc<Self> {
        Arc::new_cyclic(|self_ref| BridgeDriver {
            inner: SpinLock::new(Bridge::new(name)),
            netns: RwLock::new(Weak::new()),
            local_iface: SpinLock::new(Weak::new()),
            local_rx_queue: SpinLock::new(VecDeque::new()),
            active: AtomicBool::new(true),
            mtu_set_by_user: AtomicBool::new(false),
            in_flight: AtomicUsize::new(0),
            self_ref: self_ref.clone(),
            next_port_id: AtomicUsize::new(0),
        })
    }

    fn next_port_id(&self) -> BridgePortId {
        self.next_port_id
            .fetch_add(1, core::sync::atomic::Ordering::Relaxed)
    }

    pub(crate) fn mtu_set_by_user(&self) -> bool {
        self.mtu_set_by_user.load(Ordering::Acquire)
    }

    pub(crate) fn mark_mtu_set_by_user(&self) {
        self.mtu_set_by_user.store(true, Ordering::Release);
    }

    pub fn add_device(&self, device: Arc<dyn BridgeEnableDevice>) {
        self.try_add_device(device)
            .expect("fixture bridge port must be valid");
    }

    fn try_add_device(&self, device: Arc<dyn BridgeEnableDevice>) -> Result<(), SystemError> {
        if let Some(netns) = self.netns() {
            let port_netns = device.net_namespace().ok_or(SystemError::ENODEV)?;
            if !Arc::ptr_eq(&netns, &port_netns) {
                return Err(SystemError::EINVAL);
            }
        }
        if device.common_bridge_data().is_some() {
            return Err(SystemError::EBUSY);
        }
        let port = BridgePort::new(
            self.next_port_id(),
            device.clone(),
            &self.self_ref.upgrade().unwrap(),
        );
        log::info!("Adding port with id: {}", port.id);

        self.inner.lock().add_port(port.id, port.clone())?;
        device.set_common_bridge_data(&port);
        self.update_carrier_from_ports();
        Ok(())
    }

    pub fn remove_device(&self, device: Arc<dyn BridgeEnableDevice>) {
        self.try_remove_device(device)
            .expect("fixture bridge port must be attached");
    }

    fn try_remove_device(&self, device: Arc<dyn BridgeEnableDevice>) -> Result<(), SystemError> {
        let data = device.common_bridge_data().ok_or(SystemError::EINVAL)?;
        let owner = data
            .bridge_driver_ref
            .upgrade()
            .ok_or(SystemError::ENODEV)?;
        if !core::ptr::eq(self, owner.as_ref()) {
            return Err(SystemError::EINVAL);
        }
        let port = self
            .inner
            .lock()
            .remove_port(data.id)
            .ok_or(SystemError::ENODEV)?;
        // Never wait while holding the bridge/FDB lock: veth classification
        // enters this lock while its own ingress barrier is held.
        port.wait_idle();
        device.clear_common_bridge_data(data.id);
        self.update_carrier_from_ports();
        Ok(())
    }

    fn commit_remove_devices(&self, devices: Vec<(BridgePortId, Arc<dyn BridgeEnableDevice>)>) {
        for (port_id, device) in devices {
            // RTNL kept this prevalidated port membership unchanged.
            let port = self
                .inner
                .lock()
                .remove_port(port_id)
                .expect("prepared bridge port must still exist");
            port.wait_idle();
            device.clear_common_bridge_data(port_id);
        }
        self.update_carrier_from_ports();
    }

    /// The bridge has link carrier when at least one attached Ethernet port
    /// has carrier. Its own administrative UP state is projected separately.
    pub(crate) fn update_carrier_from_ports(&self) {
        let mut bridge = self.inner.lock();
        // A down or carrier-less port must not keep a learned unicast entry:
        // otherwise the lookup targets that port and the frame is discarded
        // instead of being flooded to a host that moved elsewhere.
        bridge.forget_inactive_ports();
        let has_carrier = self.active.load(Ordering::Acquire)
            && bridge.ports.values().any(BridgePort::can_forward);
        drop(bridge);
        if let Some(local) = self.local_iface() {
            if has_carrier {
                local.clear_net_state(NetDeivceState::__LINK_STATE_NOCARRIER);
                local.set_operstate(Operstate::IF_OPER_UP);
            } else {
                local.set_net_state(NetDeivceState::__LINK_STATE_NOCARRIER);
                local.set_operstate(Operstate::IF_OPER_DOWN);
            }
        }
    }

    pub(crate) fn port_mac_changed(
        &self,
        port_id: BridgePortId,
        old: EthernetAddress,
        new: EthernetAddress,
    ) {
        let mut bridge = self.inner.lock();
        if bridge.ports.contains_key(&port_id) {
            if let Some(count) = bridge.port_local_macs.get_mut(&old) {
                *count -= 1;
                if *count == 0 {
                    bridge.port_local_macs.remove(&old);
                }
            }
            *bridge.port_local_macs.entry(new).or_insert(0) += 1;
            bridge.forget_macs(old, new);
        }
    }

    pub fn handle_frame(&self, ingress_port_id: BridgePortId, frame: &[u8]) {
        let Some(_activity) = self.enter_data_path() else {
            return;
        };
        let decision = self.inner.lock().select_egress(ingress_port_id, frame);
        let Ok(decision) = decision else {
            return;
        };
        if decision.local {
            self.enqueue_local(frame, decision.local_stack_dst);
        }
        self.transmit_egress(decision.egress, frame);
    }

    fn transmit_egress(&self, egress: BridgeEgress, frame: &[u8]) {
        match egress {
            BridgeEgress::None => {}
            BridgeEgress::One(port) => port.transmit(frame),
            BridgeEgress::Flood(ports) => {
                for port in &ports {
                    port.transmit(frame);
                }
            }
        }
    }

    fn enqueue_local(&self, frame: &[u8], local_stack_dst: Option<EthernetAddress>) {
        let Some(local) = self.local_iface() else {
            return;
        };
        let local_iface: Arc<dyn Iface> = local.clone();
        let packet_type = if local_stack_dst.is_some() {
            crate::net::socket::packet::PacketType::Host
        } else {
            crate::net::socket::packet::classify_packet(frame, &local_iface)
        };
        // AF_PACKET observes the original Ethernet header. Only the private
        // smoltcp input copy needs the bridge MAC to pass its L2 admission
        // check for a local FDB entry owned by a port.
        crate::net::socket::packet::deliver_to_packet_sockets(&local_iface, frame, packet_type);
        let mut owned = Vec::new();
        if owned.try_reserve_exact(frame.len()).is_err() {
            return;
        }
        owned.extend_from_slice(frame);
        if let Some(dst) = local_stack_dst {
            owned[..6].copy_from_slice(dst.as_bytes());
        }
        let mut queue = self.local_rx_queue.lock();
        if queue.len() >= BRIDGE_MAX_LOCAL_FRAMES || queue.try_reserve(1).is_err() {
            return;
        }
        queue.push_back(owned);
        drop(queue);
        if let Some(napi) = local.napi_struct() {
            napi_schedule(napi);
        }
    }

    fn local_transmit(&self, frame: &[u8]) -> Result<(), SystemError> {
        let _activity = self.enter_data_path().ok_or(SystemError::ENETDOWN)?;
        let vlan_header = frame.len() >= 18
            && matches!(u16::from_be_bytes([frame[12], frame[13]]), 0x8100 | 0x88a8);
        let max_frame = self
            .local_iface()
            .map(|iface| iface.mtu() + 14)
            .unwrap_or(BRIDGE_MTU + 14)
            + if vlan_header { 4 } else { 0 };
        if frame.len() > max_frame {
            return Err(SystemError::EMSGSIZE);
        }
        let frame = EthernetFrame::new_checked(frame).map_err(|_| SystemError::EINVAL)?;
        if let Some(iface) = self.local_iface() {
            crate::net::socket::packet::deliver_to_packet_sockets(
                &(iface as Arc<dyn Iface>),
                frame.as_ref(),
                crate::net::socket::packet::PacketType::Outgoing,
            );
        }
        let dst = frame.dst_addr();
        let egress = {
            let mut bridge = self.inner.lock();
            bridge.sweep_if_due();
            if bridge.local_mac == Some(dst) || bridge.port_local_macs.contains_key(&dst) {
                BridgeEgress::None
            } else if dst.is_broadcast() || dst.as_bytes()[0] & 1 != 0 {
                bridge.flood_ports(None)?
            } else if let Some(entry) = bridge.mac_table.get(&dst) {
                bridge
                    .ports
                    .get(&entry.port_id)
                    .cloned()
                    .map_or(BridgeEgress::None, BridgeEgress::One)
            } else {
                bridge.flood_ports(None)?
            }
        };
        self.transmit_egress(egress, frame.as_ref());
        Ok(())
    }

    pub fn local_iface(&self) -> Option<Arc<BridgeIface>> {
        self.local_iface.lock().upgrade()
    }

    fn enter_data_path(&self) -> Option<BridgeActivity<'_>> {
        self.in_flight.fetch_add(1, Ordering::SeqCst);
        if !self.active.load(Ordering::SeqCst) {
            self.in_flight.fetch_sub(1, Ordering::SeqCst);
            return None;
        }
        Some(BridgeActivity { bridge: self })
    }

    fn wait_idle(&self) {
        while self.in_flight.load(Ordering::SeqCst) != 0 {
            crate::sched::sched_yield();
        }
    }

    pub fn detach_all_ports(&self) -> Result<(), SystemError> {
        loop {
            let next = self
                .inner
                .lock()
                .ports
                .values()
                .next()
                .map(|port| port.bridge_enable.clone());
            let Some(device) = next else {
                return Ok(());
            };
            self.try_remove_device(device)?;
        }
    }

    pub fn name(&self) -> String {
        self.inner.lock().name().to_string()
    }

    pub fn set_netns(&self, netns: &Arc<NetNamespace>) {
        *self.netns.write() = Arc::downgrade(netns);
    }

    pub fn clear_netns(&self) {
        *self.netns.write() = Weak::new();
    }

    pub fn netns(&self) -> Option<Arc<NetNamespace>> {
        self.netns.read().upgrade()
    }
}

struct BridgeActivity<'a> {
    bridge: &'a BridgeDriver,
}

impl Drop for BridgeActivity<'_> {
    fn drop(&mut self) {
        self.bridge.in_flight.fetch_sub(1, Ordering::SeqCst);
    }
}

/// 可供桥接设备应该实现的 trait
pub trait BridgeEnableDevice: Iface {
    /// 接收来自桥的数据帧
    fn receive_from_bridge(&self, frame: &[u8]);

    /// 设置桥接相关的公共数据
    fn set_common_bridge_data(&self, _port: &BridgePort);

    /// 获取桥接相关的公共数据
    fn common_bridge_data(&self) -> Option<BridgeCommonData>;
    fn clear_common_bridge_data(&self, bridge_id: BridgePortId);
    // fn bridge(&self) -> Weak<BridgeIface> {
    //     let Some(data) = self.common_bridge_data() else {
    //         return Weak::default();
    //     };
    //     data.bridge_driver
    // }
}

#[derive(Debug, Clone)]
pub struct BridgeCommonData {
    pub id: BridgePortId,
    pub bridge_driver_ref: Weak<BridgeDriver>,
}

fn bridge_probe() {
    let (iface1, iface2) = VethInterface::new_pair("veth_a", "veth_b");
    let (iface3, iface4) = VethInterface::new_pair("veth_c", "veth_d");

    let addr1 = IpAddress::v4(200, 0, 0, 1);
    let cidr1 = IpCidr::new(addr1, 24);
    let addr2 = IpAddress::v4(200, 0, 0, 2);
    let cidr2 = IpCidr::new(addr2, 24);

    let addr3 = IpAddress::v4(200, 0, 0, 3);
    let cidr3 = IpCidr::new(addr3, 24);
    let addr4 = IpAddress::v4(200, 0, 0, 4);
    let cidr4 = IpCidr::new(addr4, 24);

    crate::net::address::initialize_address(&(iface1.clone() as Arc<dyn Iface>), cidr1)
        .expect("initialize veth address");
    crate::net::address::initialize_address(&(iface2.clone() as Arc<dyn Iface>), cidr2)
        .expect("initialize veth address");
    crate::net::address::initialize_address(&(iface3.clone() as Arc<dyn Iface>), cidr3)
        .expect("initialize veth address");
    crate::net::address::initialize_address(&(iface4.clone() as Arc<dyn Iface>), cidr4)
        .expect("initialize veth address");

    // iface1.add_direct_route(cidr4, addr2);

    let turn_on = |a: &Arc<VethInterface>| {
        a.set_net_state(NetDeivceState::__LINK_STATE_START);
        a.set_operstate(Operstate::IF_OPER_UP);
        // NET_DEVICES.write_irqsave().insert(a.nic_id(), a.clone());
        register_netdevice(&INIT_NET_NAMESPACE, a.clone())
            .expect("register bridge fixture interface in root netns");
    };

    turn_on(&iface1);
    turn_on(&iface2);
    turn_on(&iface3);
    turn_on(&iface4);

    let bridge = BridgeDriver::new("bridge0");
    bridge.set_netns(&INIT_NET_NAMESPACE);
    INIT_NET_NAMESPACE.insert_bridge(bridge.clone());

    bridge.add_device(iface3);
    bridge.add_device(iface2);

    log::info!("Bridge device created");
}

#[unified_init(INITCALL_DEVICE)]
pub fn bridge_init() -> Result<(), SystemError> {
    if super::net_test_fixtures_enabled() {
        bridge_probe();
    }
    Ok(())
}
