use super::*;

/// MAC地址表老化时间
const MAC_ENTRY_TIMEOUT: u64 = 300_000; // 5分钟
const MAC_SWEEP_INTERVAL_MS: u64 = 1_000;
const BRIDGE_MAX_FDB_ENTRIES: usize = 4_096;
pub(super) const BRIDGE_MAX_LOCAL_FRAMES: usize = 1_024;
pub(super) const BRIDGE_MTU: usize = 1_500;

pub type BridgePortId = usize;

#[derive(Debug)]
pub(super) struct MacEntry {
    pub(super) port_id: BridgePortId,
    last_seen: Instant,
}

impl MacEntry {
    pub fn new(port: BridgePortId) -> Self {
        MacEntry {
            port_id: port,
            last_seen: Instant::now(),
        }
    }
}

/// 代表一个加入bridge的网络接口
#[derive(Debug, Clone)]
pub struct BridgePort {
    pub id: BridgePortId,
    pub(super) bridge_enable: Arc<dyn BridgeEnableDevice>,
    pub(in crate::driver::net) bridge_driver_ref: Weak<BridgeDriver>,
    pub(super) active: Arc<AtomicBool>,
    in_flight: Arc<AtomicUsize>,
    // 当前接口状态？forwarding, learning, blocking?
    // mac mtu信息
}

impl BridgePort {
    pub(super) fn new(
        id: BridgePortId,
        device: Arc<dyn BridgeEnableDevice>,
        bridge: &Arc<BridgeDriver>,
    ) -> Self {
        let port = BridgePort {
            id,
            bridge_enable: device.clone(),
            bridge_driver_ref: Arc::downgrade(bridge),
            active: Arc::new(AtomicBool::new(true)),
            in_flight: Arc::new(AtomicUsize::new(0)),
        };

        port
    }

    pub(super) fn transmit(&self, frame: &[u8]) {
        // The admission increment, active check, detach store and final
        // count check must share one global order; Acquire/Release alone
        // permits both sides to observe the other's old value.
        self.in_flight.fetch_add(1, Ordering::SeqCst);
        if self.can_forward() {
            self.bridge_enable.receive_from_bridge(frame);
        }
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
    }

    pub(super) fn can_forward(&self) -> bool {
        self.active.load(Ordering::SeqCst)
            && self.bridge_enable.flags().contains(InterfaceFlags::UP)
            && {
                let state = self.bridge_enable.net_state();
                state.contains(NetDeivceState::__LINK_STATE_START)
                    && !state.contains(NetDeivceState::__LINK_STATE_NOCARRIER)
            }
    }

    pub(super) fn wait_idle(&self) {
        while self.in_flight.load(Ordering::SeqCst) != 0 {
            crate::sched::sched_yield();
        }
    }
}

#[derive(Debug)]
pub struct Bridge {
    name: String,
    pub(super) ports: HashMap<BridgePortId, BridgePort>,
    // FDB（Forwarding Database）
    pub(super) mac_table: HashMap<EthernetAddress, MacEntry>,
    // FIFO replacement bounds memory without a full-table scan on each new
    // source MAC once the FDB is full. Existing entries keep their position.
    learn_order: VecDeque<EthernetAddress>,
    pub(super) local_mac: Option<EthernetAddress>,
    pub(super) port_local_macs: HashMap<EthernetAddress, usize>,
    last_sweep: Instant,
    // 配置参数，比如aging timeout, max age, hello time, forward delay
    // bridge_mac: EthernetAddress,
}

/// Egress ports are snapshotted while holding the FDB lock. Device receive
/// callbacks and NAPI scheduling run only after that lock is released.
pub(super) enum BridgeEgress {
    None,
    One(BridgePort),
    Flood(Vec<BridgePort>),
}

pub(super) struct BridgeDecision {
    pub(super) local: bool,
    /// A port's own MAC is a local FDB entry too. The bridge admits it, but
    /// smoltcp's Ethernet endpoint only accepts its configured bridge MAC.
    pub(super) local_stack_dst: Option<EthernetAddress>,
    pub(super) egress: BridgeEgress,
}

impl Bridge {
    pub fn new(name: &str) -> Self {
        Self {
            name: name.into(),
            ports: HashMap::new(),
            mac_table: HashMap::new(),
            learn_order: VecDeque::new(),
            local_mac: None,
            port_local_macs: HashMap::new(),
            last_sweep: Instant::now(),
        }
    }

    pub fn add_port(&mut self, id: BridgePortId, port: BridgePort) -> Result<(), SystemError> {
        self.ports.try_reserve(1).map_err(|_| SystemError::ENOMEM)?;
        // Reserve for the worst case where every attached port later gets a
        // distinct MAC. MAC changes can then update this map without an OOM
        // hazard after the device address has changed.
        let extra_local_macs = self
            .ports
            .len()
            .checked_add(1)
            .and_then(|count| count.checked_sub(self.port_local_macs.len()))
            .ok_or(SystemError::ENOMEM)?;
        self.port_local_macs
            .try_reserve(extra_local_macs)
            .map_err(|_| SystemError::ENOMEM)?;
        let mac = port.bridge_enable.mac();
        *self.port_local_macs.entry(mac).or_insert(0) += 1;
        self.ports.insert(id, port);
        Ok(())
    }

    pub fn remove_port(&mut self, port_id: BridgePortId) -> Option<BridgePort> {
        let port = self.ports.remove(&port_id);
        if let Some(port) = &port {
            port.active.store(false, Ordering::SeqCst);
        }
        // 清理MAC地址表中与该端口相关的条目
        self.mac_table
            .retain(|_mac, entry| entry.port_id != port_id);
        self.retain_learned_order();
        if let Some(port) = &port {
            let mac = port.bridge_enable.mac();
            if let Some(count) = self.port_local_macs.get_mut(&mac) {
                *count -= 1;
                if *count == 0 {
                    self.port_local_macs.remove(&mac);
                }
            }
        }
        port
    }

    fn insert_or_update_mac_entry(&mut self, src_mac: EthernetAddress, port_id: BridgePortId) {
        let bytes = src_mac.as_bytes();
        if bytes[0] & 1 != 0
            || bytes.iter().all(|byte| *byte == 0)
            || self.local_mac == Some(src_mac)
            || self.port_local_macs.contains_key(&src_mac)
        {
            return;
        }
        if let Some(entry) = self.mac_table.get_mut(&src_mac) {
            entry.port_id = port_id;
            entry.last_seen = Instant::now();
        } else {
            let full = self.mac_table.len() == BRIDGE_MAX_FDB_ENTRIES;
            if !full
                && (self.mac_table.try_reserve(1).is_err()
                    || self.learn_order.try_reserve(1).is_err())
            {
                return;
            }
            if full {
                let Some(oldest) = self.learn_order.pop_front() else {
                    debug_assert!(false, "FDB and learning order must agree");
                    return;
                };
                self.mac_table.remove(&oldest);
            }
            self.mac_table.insert(src_mac, MacEntry::new(port_id));
            self.learn_order.push_back(src_mac);
        }
    }

    fn retain_learned_order(&mut self) {
        self.learn_order
            .retain(|mac| self.mac_table.contains_key(mac));
    }

    pub(super) fn forget_macs(&mut self, old: EthernetAddress, new: EthernetAddress) {
        self.mac_table.remove(&old);
        self.mac_table.remove(&new);
        self.retain_learned_order();
    }

    pub(super) fn forget_inactive_ports(&mut self) {
        let ports = &self.ports;
        self.mac_table.retain(|_, entry| {
            ports
                .get(&entry.port_id)
                .is_some_and(BridgePort::can_forward)
        });
        self.retain_learned_order();
    }

    pub(super) fn select_egress(
        &mut self,
        ingress_port_id: BridgePortId,
        frame: &[u8],
    ) -> Result<BridgeDecision, SystemError> {
        if frame.len() < 14 {
            // 使用 smoltcp 提供的最小长度
            // log::warn!("Bridge {}: Received malformed Ethernet frame (too short).", self.name);
            return Ok(BridgeDecision {
                local: false,
                local_stack_dst: None,
                egress: BridgeEgress::None,
            });
        }

        let ether_frame = match EthernetFrame::new_checked(frame) {
            Ok(f) => f,
            Err(_) => {
                // log::warn!("Bridge {}: Received malformed Ethernet frame.", self.name);
                return Ok(BridgeDecision {
                    local: false,
                    local_stack_dst: None,
                    egress: BridgeEgress::None,
                });
            }
        };
        if !self.ports.contains_key(&ingress_port_id) {
            return Ok(BridgeDecision {
                local: false,
                local_stack_dst: None,
                egress: BridgeEgress::None,
            });
        }

        self.sweep_if_due();

        let dst_mac = ether_frame.dst_addr();
        let src_mac = ether_frame.src_addr();

        self.insert_or_update_mac_entry(src_mac, ingress_port_id);

        let port_local = self.port_local_macs.contains_key(&dst_mac);
        let local = dst_mac.is_broadcast()
            || dst_mac.as_bytes()[0] & 1 != 0
            || self.local_mac == Some(dst_mac)
            || port_local;
        let egress = if self.local_mac == Some(dst_mac) || port_local {
            Ok(BridgeEgress::None)
        } else if dst_mac.is_broadcast() || dst_mac.as_bytes()[0] & 1 != 0 {
            self.flood_ports(Some(ingress_port_id))
        } else {
            if let Some(entry) = self.mac_table.get(&dst_mac) {
                let target_port = entry.port_id;
                Ok(if target_port == ingress_port_id {
                    BridgeEgress::None
                } else {
                    self.ports
                        .get(&target_port)
                        .cloned()
                        .map_or(BridgeEgress::None, BridgeEgress::One)
                })
            } else {
                self.flood_ports(Some(ingress_port_id))
            }
        };

        Ok(BridgeDecision {
            local,
            local_stack_dst: if port_local { self.local_mac } else { None },
            egress: egress?,
        })
    }

    pub(super) fn flood_ports(
        &self,
        except_port_id: Option<BridgePortId>,
    ) -> Result<BridgeEgress, SystemError> {
        let mut eligible = self
            .ports
            .iter()
            .filter(|(id, _)| Some(**id) != except_port_id)
            .map(|(_, port)| port);
        let Some(first) = eligible.next() else {
            return Ok(BridgeEgress::None);
        };
        let Some(second) = eligible.next() else {
            return Ok(BridgeEgress::One(first.clone()));
        };
        let mut ports = Vec::new();
        ports
            .try_reserve(self.ports.len())
            .map_err(|_| SystemError::ENOMEM)?;
        ports.push(first.clone());
        ports.push(second.clone());
        ports.extend(eligible.cloned());
        Ok(BridgeEgress::Flood(ports))
    }

    pub fn sweep_mac_table(&mut self) {
        let now = Instant::now();
        self.mac_table.retain(|_mac, entry| {
            now.duration_since(entry.last_seen)
                .unwrap_or_default()
                .total_millis()
                < MAC_ENTRY_TIMEOUT
        });
        self.retain_learned_order();
    }

    pub(super) fn sweep_if_due(&mut self) {
        let now = Instant::now();
        if now
            .duration_since(self.last_sweep)
            .unwrap_or_default()
            .total_millis()
            >= MAC_SWEEP_INTERVAL_MS
        {
            self.sweep_mac_table();
            self.last_sweep = now;
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ethernet_frame(dst: EthernetAddress, src: EthernetAddress) -> [u8; 14] {
        let mut frame = [0u8; 14];
        frame[..6].copy_from_slice(dst.as_bytes());
        frame[6..12].copy_from_slice(src.as_bytes());
        frame[12..].copy_from_slice(&0x0806u16.to_be_bytes());
        frame
    }

    #[test]
    fn forwarding_respects_local_unknown_and_port_lifecycle() {
        let bridge = BridgeDriver::new("testbr");
        let (first, _first_peer) = VethInterface::new_pair_dynamic("test-a", "test-ap");
        let (second, _second_peer) = VethInterface::new_pair_dynamic("test-b", "test-bp");
        bridge
            .try_add_device(first.clone() as Arc<dyn BridgeEnableDevice>)
            .unwrap();
        bridge
            .try_add_device(second.clone() as Arc<dyn BridgeEnableDevice>)
            .unwrap();
        let first_id = first.common_bridge_data().unwrap().id;
        let second_id = second.common_bridge_data().unwrap().id;
        let local = EthernetAddress([0x02, 0xa1, 0, 0, 0, 1]);
        bridge.inner.lock().local_mac = Some(local);
        let source = EthernetAddress([0x02, 0xb1, 0, 0, 0, 1]);
        let unknown = EthernetAddress([0x02, 0xc1, 0, 0, 0, 1]);

        let decision = bridge
            .inner
            .lock()
            .select_egress(first_id, &ethernet_frame(unknown, source))
            .unwrap();
        assert!(!decision.local);
        assert!(matches!(decision.egress, BridgeEgress::One(ref port) if port.id == second_id));

        let decision = bridge
            .inner
            .lock()
            .select_egress(first_id, &ethernet_frame(local, source))
            .unwrap();
        assert!(decision.local);
        assert!(matches!(decision.egress, BridgeEgress::None));

        let decision = bridge
            .inner
            .lock()
            .select_egress(
                first_id,
                &ethernet_frame(EthernetAddress::BROADCAST, source),
            )
            .unwrap();
        assert!(decision.local);
        assert!(matches!(decision.egress, BridgeEgress::One(ref port) if port.id == second_id));

        let learned = EthernetAddress([0x02, 0xd1, 0, 0, 0, 1]);
        bridge
            .inner
            .lock()
            .select_egress(second_id, &ethernet_frame(unknown, learned))
            .unwrap();
        let decision = bridge
            .inner
            .lock()
            .select_egress(second_id, &ethernet_frame(learned, source))
            .unwrap();
        assert!(matches!(decision.egress, BridgeEgress::None));

        let before = bridge.inner.lock().mac_table.len();
        bridge
            .inner
            .lock()
            .select_egress(
                first_id,
                &ethernet_frame(unknown, EthernetAddress::BROADCAST),
            )
            .unwrap();
        assert_eq!(bridge.inner.lock().mac_table.len(), before);

        let snapshot = bridge
            .inner
            .lock()
            .select_egress(first_id, &ethernet_frame(learned, source))
            .unwrap();
        let BridgeEgress::One(port) = snapshot.egress else {
            panic!("learned unicast must target second port")
        };
        bridge
            .try_remove_device(second as Arc<dyn BridgeEnableDevice>)
            .unwrap();
        port.transmit(&ethernet_frame(learned, source));
        assert!(!port.active.load(Ordering::Acquire));
        assert_eq!(port.in_flight.load(Ordering::Acquire), 0);
    }

    #[test]
    fn fdb_capacity_replaces_old_entries_without_stopping_learning() {
        let mut bridge = Bridge::new("testbr");
        for value in 0..=BRIDGE_MAX_FDB_ENTRIES {
            let source = EthernetAddress([
                0x02,
                (value >> 24) as u8,
                (value >> 16) as u8,
                (value >> 8) as u8,
                value as u8,
                1,
            ]);
            bridge.insert_or_update_mac_entry(source, 1);
        }
        assert_eq!(bridge.mac_table.len(), BRIDGE_MAX_FDB_ENTRIES);
        assert_eq!(bridge.learn_order.len(), BRIDGE_MAX_FDB_ENTRIES);
        assert!(!bridge
            .mac_table
            .contains_key(&EthernetAddress([0x02, 0, 0, 0, 0, 1])));
        assert!(bridge
            .mac_table
            .contains_key(&EthernetAddress([0x02, 0, 0, 16, 0, 1])));
    }
}
