use core::net::Ipv4Addr;
use crate::driver::base::kobject::KObject;
use crate::driver::net::route_iface::RouteInterface;
use crate::driver::net::route_iface::RoutingAction;
use crate::driver::net::Iface;
use crate::libs::spinlock::SpinLock;
use crate::libs::wait_queue::WaitQueue;
use crate::net::routing::routing_table::RouteTable;
use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::string::ToString;
use alloc::sync::Arc;
use alloc::sync::Weak;
use alloc::vec::Vec;
use hashbrown::HashMap;
use smoltcp::wire::EthernetFrame;
use smoltcp::wire::EthernetProtocol;
use smoltcp::wire::Ipv4Packet;

const ROUTER_NAME: &str = "router";

pub struct Router {
    name: String,
    route_table: RouteTable,
    pub interfaces: HashMap<String, Arc<RouteInterface>>,
    self_ref: Weak<Self>,
    rx_buffer: SpinLock<VecDeque<Vec<u8>>>,
    wait_queue: WaitQueue,
}

impl Router {
    pub fn new() -> Arc<Self> {
        Arc::new_cyclic(|me| Router {
            name: ROUTER_NAME.to_string(),
            route_table: RouteTable::new(),
            interfaces: HashMap::new(),
            self_ref: me.clone(),
            rx_buffer: SpinLock::new(VecDeque::new()),
            wait_queue: WaitQueue::default(),
        })
    }

    pub fn add_interface(&mut self, iface: Arc<RouteInterface>) {
        iface.attach_router(self.self_ref.upgrade().unwrap());
        self.interfaces.insert(iface.name(), iface);
    }

    pub fn recv_from_iface(&self, data: Vec<u8>) {
        let mut buffer = self.rx_buffer.lock();
        buffer.push_back(data);
    }

    fn is_local_destination(&self, dst_ip: Ipv4Addr) -> bool {
        for interface in self.interfaces.values() {
            if interface.is_self_ip(dst_ip) {
                return true;
            }
        }
        false
    }

    fn route_l3_packet(&self, from_interface: &str, ip_packet: &[u8]) -> RoutingAction {
        let packet = match Ipv4Packet::new_checked(ip_packet) {
            Ok(packet) => packet,
            Err(_) => {
                log::error!("Invalid IPv4 packet received");
                return RoutingAction::Drop;
            }
        };

        let dst_ip = packet.dst_addr();

        if packet.hop_limit() <= 1 {
            log::warn!("Packet dropped due to TTL <= 1");
            return RoutingAction::Drop;
        }

        if self.is_local_destination(dst_ip) {
            return RoutingAction::DeliverToLocal;
        }

        if let Some(route) = self.route_table.lookup_route(dst_ip) {
            // 防止环路：不能从同一接口转发回去
            if route.interface.name() == from_interface {
                return RoutingAction::Drop;
            }

            // 转发到目标接口
            self.forward_l3_packet_to_interface(&route.interface.name(), ip_packet.to_vec());

            RoutingAction::Forwarded
        } else {
            RoutingAction::Drop
        }
    }

    pub fn handle_received_frame(&self, interface_name: &str, frame: &[u8]) -> RoutingAction {
        let eth_frame = match EthernetFrame::new_checked(frame) {
            Ok(frame) => frame,
            Err(_) => return RoutingAction::Drop,
        };

        let interface = match self.interfaces.get(interface_name) {
            Some(iface) => iface,
            None => return RoutingAction::Drop,
        };

        let mac = interface.mac();
        if eth_frame.dst_addr() != mac && !eth_frame.dst_addr().is_broadcast() {
            return RoutingAction::Ignore;
        }

        match eth_frame.ethertype() {
            EthernetProtocol::Ipv4 => {
                // IPv4包，进行路由处理
                self.route_l3_packet(interface_name, eth_frame.payload())
            }
            EthernetProtocol::Arp => {
                // ARP包交给本地处理
                RoutingAction::DeliverToLocal
            }
            _ => {
                // 其他协议，暂时忽略
                RoutingAction::Ignore
            }
        }
    }

    fn forward_l3_packet_to_interface(&self, target_interface: &str, mut ip_packet: Vec<u8>) {
        if let Some(interface) = self.interfaces.get(target_interface) {
            // 减少TTL
            if ip_packet.len() >= 20 {
                let mut packet = Ipv4Packet::new_unchecked(&mut ip_packet);
                let new_ttl = packet.hop_limit().saturating_sub(1);
                packet.set_hop_limit(new_ttl);

                // 重新计算校验和
                packet.fill_checksum();
            }

            // 将L3包注入到目标接口，让smoltcp处理路由和发送
            interface.inject_l3_packet_for_sending(ip_packet);
        }
    }

    pub fn poll_blocking(&self) {
        use crate::sched::SchedMode;

        loop {
            let mut inner = self.rx_buffer.lock_irqsave();

            let opt = inner.pop_front();
            if let Some(frame) = opt {
                // let mut frame = smoltcp::wire::EthernetFrame::new_unchecked(frame);
                // log::info!("Router received frame: {:?}", frame);

                // drop(inner);

                // let mut ip_packet_bytes = frame.payload_mut();
                // let mut ipv4_packet = Ipv4Packet::new_unchecked(&mut ip_packet_bytes);

                // // 1. 递减 TTL
                // let original_ttl = ipv4_packet.hop_limit();
                // if original_ttl <= 1 {
                //     // TTL 耗尽，数据包应该被丢弃，并可能发送 ICMP Time Exceeded 消息
                //     println!("TTL reached 0, dropping packet.");
                //     return;
                // }
                // ipv4_packet.set_hop_limit(original_ttl - 1);

                // ipv4_packet.fill_checksum();

                // let dest_ip = ipv4_packet.dst_addr();

                // if let Some(entry) = self.route_table.lookup_route(IpAddress::Ipv4(dest_ip)) {
                //     //todo!
                // }
            } else {
                drop(inner);
                log::info!("Router is going to sleep");
                let _ = wq_wait_event_interruptible!(
                    self.wait_queue,
                    !self.rx_buffer.lock().is_empty(),
                    {}
                );
            }
        }
    }
}
