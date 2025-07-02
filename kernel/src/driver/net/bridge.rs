use crate::{
    libs::{rwlock::RwLock, spinlock::SpinLock},
    time::Instant,
};
use alloc::{
    string::String,
    sync::{Arc, Weak},
    vec::Vec,
};
use hashbrown::HashMap;
use smoltcp::wire::EthernetAddress;

const MAC_ENTRY_TIMEOUT: u64 = 60_000; // 60秒

struct MacEntry {
    port: Arc<LockedBridgePort>,
    pub(self) record: RwLock<MacEntryRecord>,
    // 存活时间（动态学习的老化）
}

impl MacEntry {
    pub fn new(port: Arc<LockedBridgePort>) -> Self {
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
pub struct BridgePort {
    name: String,
    bridge_enable: Arc<dyn BridgeEnableDevice>,
    bridge: Weak<Bridge>,
    // 当前接口状态？forwarding, learning, blocking?
    // mac mtu信息
}

impl BridgePort {
    fn new(device: Arc<dyn BridgeEnableDevice>) -> Self {
        BridgePort {
            name: device.name(),
            bridge_enable: device,
            bridge: Weak::new(),
        }
    }
}

pub struct LockedBridgePort(pub SpinLock<BridgePort>);
unsafe impl Send for LockedBridgePort {}
unsafe impl Sync for LockedBridgePort {}

pub struct Bridge {
    name: String,
    ports: RwLock<Vec<Arc<LockedBridgePort>>>,
    // FDB（Forwarding Database）
    mac_table: RwLock<HashMap<EthernetAddress, MacEntry>>,
    // 配置参数，比如aging timeout, max age, hello time, forward delay
}

unsafe impl Send for Bridge {}
unsafe impl Sync for Bridge {}

impl Bridge {
    pub fn new(name: &str) -> Self {
        Self {
            name: name.into(),
            ports: RwLock::new(Vec::new()),
            mac_table: RwLock::new(HashMap::new()),
        }
    }

    pub fn add_port(&self, port: Arc<LockedBridgePort>) {
        self.ports.write_irqsave().push(port);
    }

    pub fn handle_frame(
        &self,
        ingress_port: Arc<LockedBridgePort>,
        frame: &[u8],
        dst_mac: EthernetAddress,
        src_mac: EthernetAddress,
    ) {
        let guard = self.mac_table.write_irqsave();
        if let Some(entry) = guard.get(&src_mac) {
            entry.update_last_seen();
        } else {
            // MAC 学习
            self.mac_table
                .write_irqsave()
                .insert(src_mac, MacEntry::new(ingress_port.clone()));
        }

        if dst_mac.is_broadcast() {
            // 广播
            self.flood(&ingress_port, frame);
        } else {
            // 单播
            if let Some(entry) = self.mac_table.read().get(&dst_mac) {
                let target_port = &entry.port;
                // 避免发回自己
                if !Arc::ptr_eq(target_port, &ingress_port) {
                    target_port
                        .0
                        .lock()
                        .bridge_enable
                        .receive_from_bridge(frame);
                }
            } else {
                // 未知单播 → 广播
                self.flood(&ingress_port, frame);
            }
        }
    }

    fn flood(&self, except_port: &Arc<LockedBridgePort>, frame: &[u8]) {
        for port in self.ports.read().iter() {
            if !Arc::ptr_eq(port, except_port) {
                port.0.lock().bridge_enable.receive_from_bridge(frame);
            }
        }
    }

    fn transmit_to(port: &BridgePort, frame: &[u8]) {
        port.bridge_enable.receive_from_bridge(frame);
    }

    pub fn sweep_mac_table(&self) {
        let now = Instant::now();
        self.mac_table.write_irqsave().retain(|_mac, entry| {
            now.duration_since(entry.record.read().last_seen)
                .unwrap()
                .total_millis()
                < MAC_ENTRY_TIMEOUT
        });
    }
}

#[derive(Clone)]
pub struct BridgeDriver {
    pub inner: Arc<Bridge>,
}

impl BridgeDriver {
    pub fn new(name: &str) -> Self {
        BridgeDriver {
            inner: Arc::new(Bridge::new(name)),
        }
    }

    pub fn add_port(&self, port: Arc<dyn BridgeEnableDevice>) {
        let bridge_port = Arc::new(LockedBridgePort(SpinLock::new(BridgePort::new(port))));
        self.inner.add_port(bridge_port.clone());
        let mut guard = bridge_port.0.lock();
        guard.bridge = Arc::downgrade(&self.inner);
    }

    pub fn handle_frame(&self, ingress_port: Arc<LockedBridgePort>, frame: &[u8]) {
        if frame.len() < 14 {
            return; // 非法以太网帧
        }

        let dst_mac = EthernetAddress::from_bytes(&frame[0..6]);
        let src_mac = EthernetAddress::from_bytes(&frame[6..12]);

        self.inner
            .handle_frame(ingress_port, frame, dst_mac, src_mac);
    }
}

/// 可供桥接设备应该实现的 trait
pub trait BridgeEnableDevice {
    fn name(&self) -> String;
    fn receive_from_bridge(&self, frame: &[u8]);
    fn mac_addr(&self) -> EthernetAddress;
}
