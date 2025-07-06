use crate::{
    driver::net::Iface,
    libs::{rwlock::RwLock, spinlock::SpinLock},
    time::Instant,
};
use alloc::{collections::BTreeMap, string::String, sync::Arc};
use hashbrown::HashMap;
use smoltcp::wire::EthernetAddress;

const MAC_ENTRY_TIMEOUT: u64 = 60_000; // 60秒

struct MacEntry {
    port: Arc<BridgePort>,
    pub(self) record: RwLock<MacEntryRecord>,
    // 存活时间（动态学习的老化）
}

impl MacEntry {
    pub fn new(port: Arc<BridgePort>) -> Self {
        MacEntry {
            port,
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

struct MacEntryRecord {
    last_seen: Instant,
}

/// 代表一个加入bridge的网络接口
#[derive(Clone)]
pub struct BridgePort {
    bridge_enable: Arc<dyn BridgeEnableDevice>,
    bridge: BridgeDriver,
    // 当前接口状态？forwarding, learning, blocking?
    // mac mtu信息
}

impl BridgePort {
    fn new(device: Arc<dyn BridgeEnableDevice>, bridge: BridgeDriver) -> Self {
        BridgePort {
            bridge_enable: device,
            bridge,
        }
    }

    fn mac(&self) -> EthernetAddress {
        self.bridge_enable.mac()
    }
}

pub struct Bridge {
    name: String,
    // 端口列表，key为MAC地址
    ports: BTreeMap<EthernetAddress, Arc<BridgePort>>,
    // FDB（Forwarding Database）
    mac_table: HashMap<EthernetAddress, MacEntry>,
    // 配置参数，比如aging timeout, max age, hello time, forward delay
}

impl Bridge {
    pub fn new(name: &str) -> Self {
        Self {
            name: name.into(),
            ports: BTreeMap::new(),
            mac_table: HashMap::new(),
        }
    }

    pub fn add_port(&mut self, port: Arc<BridgePort>) {
        self.ports.insert(port.mac(), port);
    }

    pub fn insert_macentry(&mut self, src_mac: EthernetAddress, port: Arc<BridgePort>) {
        self.mac_table.insert(src_mac, MacEntry::new(port));
    }

    pub fn handle_frame(
        &mut self,
        ingress_port: Arc<BridgePort>,
        frame: &[u8],
        dst_mac: EthernetAddress,
        src_mac: EthernetAddress,
    ) {
        if let Some(entry) = self.mac_table.get(&src_mac) {
            entry.update_last_seen();
        } else {
            // MAC 学习
            self.insert_macentry(src_mac, ingress_port.clone());
        }

        if dst_mac.is_broadcast() {
            // 广播
            self.flood(ingress_port.mac(), frame);
        } else {
            // 单播
            if let Some(entry) = self.mac_table.get(&dst_mac) {
                let target_port = &entry.port;
                // 避免发回自己
                if !Arc::ptr_eq(target_port, &ingress_port) {
                    Bridge::transmit_to(target_port, frame);
                }
            } else {
                // 未知单播 → 广播
                self.flood(ingress_port.mac(), frame);
            }
        }

        self.sweep_mac_table();
    }

    fn flood(&self, except_mac: EthernetAddress, frame: &[u8]) {
        for (mac, port) in self.ports.iter() {
            if mac != &except_mac {
                Bridge::transmit_to(port, frame);
            }
        }
    }

    fn transmit_to(port: &BridgePort, frame: &[u8]) {
        port.bridge_enable.receive_from_bridge(frame);
    }

    pub fn sweep_mac_table(&mut self) {
        let now = Instant::now();
        self.mac_table.retain(|_mac, entry| {
            now.duration_since(entry.record.read().last_seen)
                .unwrap()
                .total_millis()
                < MAC_ENTRY_TIMEOUT
        });
    }
}

#[derive(Clone)]
pub struct BridgeDriver {
    pub inner: Arc<SpinLock<Bridge>>,
}

impl BridgeDriver {
    pub fn new(name: &str) -> Self {
        BridgeDriver {
            inner: Arc::new(SpinLock::new(Bridge::new(name))),
        }
    }

    pub fn add_port(&self, port: Arc<dyn BridgeEnableDevice>) {
        let port = BridgePort::new(port, self.clone());

        let bridge_port = Arc::new(port);
        self.inner.lock().add_port(bridge_port.clone());
    }

    pub fn handle_frame(&self, ingress_port: Arc<BridgePort>, frame: &[u8]) {
        if frame.len() < 14 {
            return; // 非法以太网帧
        }

        let dst_mac = EthernetAddress::from_bytes(&frame[0..6]);
        let src_mac = EthernetAddress::from_bytes(&frame[6..12]);
        //todo Frame::new_unchecked

        self.inner
            .lock()
            .handle_frame(ingress_port, frame, dst_mac, src_mac);
    }
}

/// 可供桥接设备应该实现的 trait
pub trait BridgeEnableDevice: Iface {
    fn receive_from_bridge(&self, frame: &[u8]);
    fn transmit_to_bridge(&self, frame: &[u8]) {
        // 默认实现，子类可以覆盖
        self.receive_from_bridge(frame);
    }
}
