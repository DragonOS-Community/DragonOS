use crate::{
    driver::net::{
        napi::napi_schedule, register_netdevice, veth::VethInterface, Iface, NetDeivceState,
        Operstate,
    },
    init::initcall::INITCALL_DEVICE,
    libs::{rwlock::RwLock, spinlock::SpinLock},
    process::namespace::net_namespace::{NetNamespace, INIT_NET_NAMESPACE},
    time::Instant,
};
use alloc::string::ToString;
use alloc::sync::Weak;
use alloc::{collections::BTreeMap, string::String, sync::Arc};
use core::sync::atomic::AtomicUsize;
use hashbrown::HashMap;
use smoltcp::wire::{EthernetAddress, EthernetFrame, IpAddress, IpCidr};
use system_error::SystemError;
use unified_init::macros::unified_init;

/// MAC地址表老化时间
const MAC_ENTRY_TIMEOUT: u64 = 300_000; // 5分钟

pub type BridgePortId = usize;

#[derive(Debug)]
struct MacEntry {
    port_id: BridgePortId,
    pub(self) record: RwLock<MacEntryRecord>,
    // 存活时间（动态学习的老化）
}

impl MacEntry {
    pub fn new(port: BridgePortId) -> Self {
        MacEntry {
            port_id: port,
            record: RwLock::new(MacEntryRecord {
                last_seen: Instant::now(),
            }),
        }
    }

    /// 更新最后一次被看到的时间为现在
    pub(self) fn update_last_seen(&self) {
        self.record.write_irqsave().last_seen = Instant::now();
    }
}

#[derive(Debug)]
struct MacEntryRecord {
    last_seen: Instant,
}

/// 代表一个加入bridge的网络接口
#[derive(Debug, Clone)]
pub struct BridgePort {
    pub id: BridgePortId,
    pub(super) bridge_enable: Arc<dyn BridgeEnableDevice>,
    pub(super) bridge_driver_ref: Weak<BridgeDriver>,
    // 当前接口状态？forwarding, learning, blocking?
    // mac mtu信息
}

impl BridgePort {
    fn new(
        id: BridgePortId,
        device: Arc<dyn BridgeEnableDevice>,
        bridge: &Arc<BridgeDriver>,
    ) -> Self {
        let port = BridgePort {
            id,
            bridge_enable: device.clone(),
            bridge_driver_ref: Arc::downgrade(bridge),
        };

        device.set_common_bridge_data(&port);

        port
    }
}

#[derive(Debug)]
pub struct Bridge {
    name: String,
    // 端口列表，key为MAC地址
    ports: BTreeMap<BridgePortId, BridgePort>,
    // FDB（Forwarding Database）
    mac_table: HashMap<EthernetAddress, MacEntry>,
    // 配置参数，比如aging timeout, max age, hello time, forward delay
    // bridge_mac: EthernetAddress,
}

impl Bridge {
    pub fn new(name: &str) -> Self {
        Self {
            name: name.into(),
            ports: BTreeMap::new(),
            mac_table: HashMap::new(),
        }
    }

    pub fn add_port(&mut self, id: BridgePortId, port: BridgePort) {
        self.ports.insert(id, port);
    }

    pub fn remove_port(&mut self, port_id: BridgePortId) {
        self.ports.remove(&port_id);
        // 清理MAC地址表中与该端口相关的条目
        self.mac_table
            .retain(|_mac, entry| entry.port_id != port_id);
    }

    fn insert_or_update_mac_entry(&mut self, src_mac: EthernetAddress, port_id: BridgePortId) {
        if let Some(entry) = self.mac_table.get(&src_mac) {
            entry.update_last_seen();
            // 如果 MAC 地址学习到了不同的端口，需要更新
            if entry.port_id != port_id {
                // log::info!("Bridge {}: MAC {} moved from port {} to port {}", self.name, src_mac, entry.port_id, port_id);
                self.mac_table.insert(src_mac, MacEntry::new(port_id));
            }
        } else {
            // log::info!("Bridge {}: Learned MAC {} on port {}", self.name, src_mac, port_id);
            self.mac_table.insert(src_mac, MacEntry::new(port_id));
        }
    }

    pub fn handle_frame(&mut self, ingress_port_id: BridgePortId, frame: &[u8]) {
        if frame.len() < 14 {
            // 使用 smoltcp 提供的最小长度
            // log::warn!("Bridge {}: Received malformed Ethernet frame (too short).", self.name);
            return;
        }

        let ether_frame = match EthernetFrame::new_checked(frame) {
            Ok(f) => f,
            Err(_) => {
                // log::warn!("Bridge {}: Received malformed Ethernet frame.", self.name);
                return;
            }
        };

        let dst_mac = ether_frame.dst_addr();
        let src_mac = ether_frame.src_addr();

        self.insert_or_update_mac_entry(src_mac, ingress_port_id);

        if dst_mac.is_broadcast() {
            // 广播 这里有可能是arp请求
            self.flood(Some(ingress_port_id), frame);
        } else {
            // 单播
            if let Some(entry) = self.mac_table.get(&dst_mac) {
                let target_port = entry.port_id;
                // 避免发回自己
                // if target_port != ingress_port_id {
                self.transmit_to_port(target_port, frame);
                // }
            } else {
                // 未知单播 → 广播
                log::info!("unknown unicast, flooding frame");
                self.flood(Some(ingress_port_id), frame);
            }
        }

        self.sweep_mac_table();
    }

    fn flood(&self, except_port_id: Option<BridgePortId>, frame: &[u8]) {
        match except_port_id {
            Some(except_id) => {
                for (&port_id, bridge_port) in &self.ports {
                    if port_id != except_id {
                        self.transmit_to_device(bridge_port, frame);
                    }
                }
            }
            None => {
                for bridge_port in self.ports.values() {
                    self.transmit_to_device(bridge_port, frame);
                }
            }
        }
    }

    fn transmit_to_port(&self, target_port_id: BridgePortId, frame: &[u8]) {
        if let Some(device_arc) = self.ports.get(&target_port_id) {
            self.transmit_to_device(device_arc, frame);
        } else {
            // log::warn!("Bridge {}: Attempted to transmit to non-existent port ID {}", self.name, target_port_id);
        }
    }

    fn transmit_to_device(&self, device: &BridgePort, frame: &[u8]) {
        device.bridge_enable.receive_from_bridge(frame);
        if let Some(napi) = device.bridge_enable.napi_struct() {
            napi_schedule(napi);
        }
    }

    pub fn sweep_mac_table(&mut self) {
        let now = Instant::now();
        self.mac_table.retain(|_mac, entry| {
            now.duration_since(entry.record.read().last_seen)
                .unwrap_or_default()
                .total_millis()
                < MAC_ENTRY_TIMEOUT
        });
    }

    pub fn name(&self) -> &str {
        &self.name
    }
}

#[derive(Debug)]
pub struct BridgeDriver {
    pub inner: SpinLock<Bridge>,
    pub netns: RwLock<Weak<NetNamespace>>,
    self_ref: Weak<BridgeDriver>,
    next_port_id: AtomicUsize,
}

impl BridgeDriver {
    pub fn new(name: &str) -> Arc<Self> {
        Arc::new_cyclic(|self_ref| BridgeDriver {
            inner: SpinLock::new(Bridge::new(name)),
            netns: RwLock::new(Weak::new()),
            self_ref: self_ref.clone(),
            next_port_id: AtomicUsize::new(0),
        })
    }

    fn next_port_id(&self) -> BridgePortId {
        self.next_port_id
            .fetch_add(1, core::sync::atomic::Ordering::Relaxed)
    }

    pub fn add_device(&self, device: Arc<dyn BridgeEnableDevice>) {
        if let Some(netns) = self.netns() {
            if !Arc::ptr_eq(
                &netns,
                &device.net_namespace().unwrap_or(INIT_NET_NAMESPACE.clone()),
            ) {
                log::warn!("Port and bridge are in different net namespaces");
                return;
            }
        }
        let port = BridgePort::new(
            self.next_port_id(),
            device.clone(),
            &self.self_ref.upgrade().unwrap(),
        );
        log::info!("Adding port with id: {}", port.id);

        self.inner.lock().add_port(port.id, port);
    }

    pub fn remove_device(&self, device: Arc<dyn BridgeEnableDevice>) {
        let Some(common_data) = device.common_bridge_data() else {
            log::warn!("Device is not part of any bridge");
            return;
        };
        self.inner.lock().remove_port(common_data.id);
    }

    pub fn handle_frame(&self, ingress_port_id: BridgePortId, frame: &[u8]) {
        self.inner.lock().handle_frame(ingress_port_id, frame);
    }

    pub fn name(&self) -> String {
        self.inner.lock().name().to_string()
    }

    pub fn set_netns(&self, netns: &Arc<NetNamespace>) {
        *self.netns.write() = Arc::downgrade(netns);
    }

    pub fn netns(&self) -> Option<Arc<NetNamespace>> {
        self.netns.read().upgrade()
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

    iface1.update_ip_addrs(cidr1);
    iface2.update_ip_addrs(cidr2);
    iface3.update_ip_addrs(cidr3);
    iface4.update_ip_addrs(cidr4);

    iface1.add_default_route_to_peer(addr2);
    iface2.add_default_route_to_peer(addr1);
    iface3.add_default_route_to_peer(addr4);
    iface4.add_default_route_to_peer(addr3);

    // iface1.add_direct_route(cidr4, addr2);

    let turn_on = |a: &Arc<VethInterface>| {
        a.set_net_state(NetDeivceState::__LINK_STATE_START);
        a.set_operstate(Operstate::IF_OPER_UP);
        // NET_DEVICES.write_irqsave().insert(a.nic_id(), a.clone());
        INIT_NET_NAMESPACE.add_device(a.clone());
        a.common().set_net_namespace(INIT_NET_NAMESPACE.clone());

        register_netdevice(a.clone()).expect("register veth device failed");
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
    bridge_probe();
    // log::info!("bridge initialized.");
    Ok(())
}
