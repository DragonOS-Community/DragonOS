use crate::{libs::rwlock::RwLock, time::Instant};
use alloc::{collections::BTreeMap, string::String, sync::Arc, vec::Vec};
use smoltcp::wire::EthernetAddress;

const MAC_ENTRY_TIMEOUT: u64 = 60_000; // 60秒

struct MacEntry {
    port: String,
    last_seen: Instant,
}

pub struct BridgePort {
    name: String,
    bridge_enable: Arc<dyn BridgeEnableDevice>,
}

pub struct Bridge {
    name: String,
    ports: RwLock<BTreeMap<String, Arc<BridgePort>>>,
    mac_table: RwLock<BTreeMap<EthernetAddress, MacEntry>>,
}

impl Bridge {
    pub fn new(name: &str) -> Self {
        Self {
            name: name.into(),
            ports: RwLock::new(BTreeMap::new()),
            mac_table: RwLock::new(BTreeMap::new()),
        }
    }

    pub fn add_port(&self, port: Arc<dyn BridgeEnableDevice>) {
        let port_name = port.name();
        let port_obj = Arc::new(BridgePort {
            name: port_name.clone(),
            bridge_enable: port.clone(),
        });

        self.ports
            .write_irqsave()
            .insert(port_name.clone(), port_obj);
    }

    pub fn handle_frame(
        &self,
        src_mac: EthernetAddress,
        dst_mac: EthernetAddress,
        frame: Vec<u8>,
        ingress: &str,
    ) {
        // MAC 学习
        self.mac_table.write_irqsave().insert(
            src_mac,
            MacEntry {
                port: ingress.into(),
                last_seen: Instant::now(),
            },
        );

        let ports = self.ports.read();
        if dst_mac == EthernetAddress::BROADCAST {
            // 广播
            for (name, port) in ports.iter() {
                if name != ingress {
                    Self::transmit_to(port, &frame);
                }
            }
        } else {
            // 单播
            if let Some(out_port) = self.mac_table.read().get(&dst_mac) {
                if let Some(port) = ports.get(out_port.port.as_str()) {
                    Self::transmit_to(port, &frame);
                }
            } else {
                // 未知单播 → 广播
                for (name, port) in ports.iter() {
                    if name != ingress {
                        Self::transmit_to(port, &frame);
                    }
                }
            }
        }
    }

    fn transmit_to(port: &BridgePort, frame: &[u8]) {
        port.bridge_enable.bridge_transmit(frame);
    }

    pub fn sweep_mac_table(&self) {
        let now = Instant::now();
        self.mac_table.write_irqsave().retain(|_mac, entry| {
            now.duration_since(entry.last_seen).unwrap().total_millis() < MAC_ENTRY_TIMEOUT
        });
    }
}

pub trait BridgeEnableDevice {
    fn name(&self) -> String;
    fn bridge_transmit(&self, frame: &[u8]);
    // fn bridge_receive(&self, frame: &[u8]) ;
}
