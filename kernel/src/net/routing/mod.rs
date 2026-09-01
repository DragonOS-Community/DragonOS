use crate::driver::net::Iface;
use crate::libs::rwsem::RwSem;
use crate::net::routing::nat::ConnTracker;
use crate::net::routing::nat::DnatPolicy;
use crate::net::routing::nat::FiveTuple;
use crate::net::routing::nat::NatPktStatus;
use crate::net::routing::nat::NatPolicy;
use crate::net::routing::nat::SnatPolicy;
use crate::process::namespace::net_namespace::NetNamespace;
use crate::process::namespace::net_namespace::INIT_NET_NAMESPACE;
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use smoltcp::wire::{EthernetFrame, IpAddress, IpCidr, Ipv4Packet};
use system_error::SystemError;

mod nat;
pub mod uapi;

/// 路由决策结果
#[derive(Debug)]
pub struct RouteDecision {
    /// 出接口
    pub interface: Arc<dyn Iface>,
    /// 下一跳地址（先写在这里
    pub next_hop: IpAddress,
}

#[derive(Debug)]
pub enum IngressRouteDecision {
    Local(Arc<dyn Iface>),
    Broadcast(Arc<dyn Iface>),
    Forward(RouteDecision),
}

#[derive(Debug)]
pub struct Router {
    /// Authoritative Linux-visible route state for this network namespace.
    pub(in crate::net) fib: RwSem<crate::net::route::FibTable>,
    pub(self) nat_tracker: Arc<ConnTracker>,
    pub ns: RwSem<Weak<NetNamespace>>,
}

impl Router {
    pub fn new(_name: String) -> Arc<Self> {
        Arc::new(Self {
            fib: RwSem::new(crate::net::route::FibTable::default()),
            nat_tracker: Arc::new(ConnTracker::default()),
            ns: RwSem::new(Weak::default()),
        })
    }

    /// 创建一个空的Router实例，主要用于初始化网络命名空间时使用
    /// 注意： 这个Router实例不会启动轮询线程
    pub fn new_empty() -> Arc<Self> {
        Arc::new(Self {
            fib: RwSem::new(crate::net::route::FibTable::default()),
            ns: RwSem::new(Weak::default()),
            nat_tracker: Arc::new(ConnTracker::default()),
        })
    }

    pub fn lookup_ingress_route(
        &self,
        dest_ip: IpAddress,
        ingress_oif: u32,
    ) -> Option<IngressRouteDecision> {
        let netns = self.ns.read().upgrade()?;
        let decision = crate::net::route::lookup_ingress(&netns, dest_ip, ingress_oif)?;
        let interface = netns.device_list().get(&(decision.oif as usize)).cloned()?;
        match decision.matched.kind {
            crate::net::route::RTN_LOCAL => Some(IngressRouteDecision::Local(interface)),
            crate::net::route::RTN_BROADCAST => Some(IngressRouteDecision::Broadcast(interface)),
            crate::net::route::RTN_UNICAST
                if interface
                    .flags()
                    .contains(crate::driver::net::types::InterfaceFlags::UP) =>
            {
                Some(IngressRouteDecision::Forward(RouteDecision {
                    interface,
                    next_hop: decision.next_hop,
                }))
            }
            _ => None,
        }
    }

    pub fn nat_tracker(&self) -> Arc<ConnTracker> {
        self.nat_tracker.clone()
    }
}

/// 获取初始化网络命名空间下的路由表
pub fn init_netns_router() -> Arc<Router> {
    INIT_NET_NAMESPACE.router().clone()
}

/// 可供路由设备应该实现的 trait
pub trait RouterEnableDevice: Iface {
    /// # 网卡处理可路由的包
    /// ## 参数
    /// - `packet`: 需要处理的以太网帧
    /// ## 返回值
    /// - `Ok(())`: 通过路由处理成功
    /// - `Err(None)`: 忽略非IPv4包或没有路由到达的包，告诉外界没有经过处理，应该交由网卡进行默认处理
    /// - `Err(Some(SystemError))`: 处理失败，可能是包格式错误或其他系统错误
    fn handle_routable_packet(
        &self,
        ether_frame: &EthernetFrame<&[u8]>,
    ) -> Result<(), Option<SystemError>> {
        match ether_frame.ethertype() {
            smoltcp::wire::EthernetProtocol::Ipv4 => {
                // 获取IPv4包的可变引用
                let mut payload_mut = ether_frame.payload().to_vec();
                let mut ipv4_packet_mut =
                    Ipv4Packet::new_checked(&mut payload_mut).map_err(|e| {
                        log::warn!("Invalid IPv4 packet: {:?}", e);
                        Some(SystemError::EINVAL)
                    })?;

                let maybe_tuple = FiveTuple::extract_from_ipv4_packet(
                    &Ipv4Packet::new_checked(ether_frame.payload()).unwrap(),
                );

                // === PRE-ROUTING HOOK ===

                let pkt_status = self.pre_routing_hook(&maybe_tuple, &mut ipv4_packet_mut);
                ipv4_packet_mut.fill_checksum();

                // === PRE-ROUTING HOOK END ===

                let dst_ip = ipv4_packet_mut.dst_addr();

                // Classify through the namespace local table before the main
                // table, just like Linux ingress routing. Local destinations
                // must never re-enter the forwarding path.
                let router = self.netns_router();

                let decision =
                    match router.lookup_ingress_route(dst_ip.into(), self.nic_id() as u32) {
                        Some(d) => d,
                        None => {
                            log::warn!("No route to {}", dst_ip);
                            return Err(None);
                        }
                    };

                drop(router);

                let decision = match decision {
                    IngressRouteDecision::Local(interface) => {
                        if interface.nic_id() == self.nic_id() {
                            return Err(None);
                        }
                        interface
                            .inject_local_ipv4_packet(
                                self.nic_id() as u32,
                                ether_frame.src_addr(),
                                ipv4_packet_mut.as_ref(),
                                false,
                            )
                            .map_err(Some)?;
                        return Ok(());
                    }
                    IngressRouteDecision::Broadcast(interface) => {
                        if interface.nic_id() == self.nic_id() {
                            return Err(None);
                        }
                        interface
                            .inject_local_ipv4_packet(
                                self.nic_id() as u32,
                                ether_frame.src_addr(),
                                ipv4_packet_mut.as_ref(),
                                true,
                            )
                            .map_err(Some)?;
                        return Ok(());
                    }
                    IngressRouteDecision::Forward(decision) => decision,
                };

                // TTL is consumed only by forwarding, never by local input.
                if ipv4_packet_mut.hop_limit() <= 1 {
                    log::warn!("TTL exceeded for packet to {}", dst_ip);
                    return Err(Some(SystemError::EINVAL));
                }

                // === POST-ROUTING HOOK ===

                self.post_routing_hook(&maybe_tuple, &mut ipv4_packet_mut, &pkt_status);
                ipv4_packet_mut.fill_checksum();

                // === POST-ROUTING HOOK END ===

                // 检查是否是从同一个接口进来又要从同一个接口出去（避免回路）
                if self.iface_name() == decision.interface.iface_name() {
                    log::info!(
                        "Ignoring packet loop from {} to {}",
                        self.iface_name(),
                        dst_ip
                    );
                    return Err(None);
                }

                // 创建修改后的IP包（递减TTL）
                sub_ttl_ipv4(&mut ipv4_packet_mut);
                ipv4_packet_mut.fill_checksum();

                // 交给出接口进行发送
                let next_hop = &decision.next_hop;
                decision
                    .interface
                    .route_and_send(next_hop, ipv4_packet_mut.as_ref())
                    .map_err(Some)?;

                // log::info!("Routed packet from {} to {} ", self.iface_name(), dst_ip);
                Ok(())
            }
            smoltcp::wire::EthernetProtocol::Arp => {
                // 忽略ARP包
                // log::info!(
                //     "Ignoring non-IPv4 packet on interface {}",
                //     self.iface_name()
                // );
                Err(None)
            }
            smoltcp::wire::EthernetProtocol::Ipv6 => {
                log::warn!("IPv6 is not supported yet, ignoring packet");
                Err(None)
            }
            _ => {
                log::warn!(
                    "Unknown ethertype {:?}, ignoring packet",
                    ether_frame.ethertype()
                );
                Err(None)
            }
        }
    }

    fn pre_routing_hook(
        &self,
        tuple: &Option<FiveTuple>,
        ipv4_packet_mut: &mut Ipv4Packet<&mut Vec<u8>>,
    ) -> NatPktStatus {
        let Some(tuple) = tuple else {
            return NatPktStatus::Untouched;
        };

        let tracker = self.netns_router().nat_tracker();

        if let Some((new_dst_ip, new_dst_port)) = tracker.snat.lock().process_return_traffic(tuple)
        {
            // log::info!(
            //     "Reverse SNAT: Translating {}:{} to {}:{}",
            //     tuple.src_addr,
            //     tuple.src_port,
            //     new_dst_ip,
            //     new_dst_port
            // );

            SnatPolicy::update_dst(
                tuple.src_addr,
                new_dst_ip,
                new_dst_port,
                tuple.protocol,
                ipv4_packet_mut,
            );

            let new_tuple = FiveTuple {
                dst_addr: new_dst_ip,
                dst_port: new_dst_port,
                src_addr: tuple.src_addr,
                src_port: tuple.src_port,
                protocol: tuple.protocol,
            };

            return NatPktStatus::ReverseSnat(new_tuple);
        }

        let mut dnat_guard = tracker.dnat.lock();
        if let Some((new_dst_ip, new_dst_port)) = dnat_guard.process_new_connection(tuple) {
            // log::info!(
            //     "DNAT: Translating {}:{} to {}:{}",
            //     tuple.dst_addr,
            //     tuple.dst_port,
            //     new_dst_ip,
            //     new_dst_port
            // );

            DnatPolicy::update_dst(
                tuple.src_addr,
                new_dst_ip,
                new_dst_port,
                tuple.protocol,
                ipv4_packet_mut,
            );

            let new_tuple = FiveTuple {
                dst_addr: new_dst_ip,
                dst_port: new_dst_port,
                src_addr: tuple.src_addr,
                src_port: tuple.src_port,
                protocol: tuple.protocol,
            };

            return NatPktStatus::NewDnat(new_tuple);
        }

        return NatPktStatus::Untouched;
    }

    fn post_routing_hook(
        &self,
        tuple: &Option<FiveTuple>,
        ipv4_packet_mut: &mut Ipv4Packet<&mut Vec<u8>>,
        pkt_status: &NatPktStatus,
    ) {
        let tuple = match pkt_status {
            NatPktStatus::ReverseSnat(t) => t,
            NatPktStatus::NewDnat(t) => t,
            NatPktStatus::Untouched => {
                let Some(tuple) = tuple else {
                    return;
                };
                tuple
            }
        };

        let tracker = self.netns_router().nat_tracker();

        if let Some((new_src_ip, new_src_port)) = tracker.dnat.lock().process_return_traffic(tuple)
        {
            // log::info!(
            //     "Reverse DNAT: Translating src {}:{} -> {}:{}",
            //     tuple.src_addr,
            //     tuple.src_port,
            //     new_src_ip,
            //     new_src_port
            // );

            DnatPolicy::update_src(
                tuple.dst_addr,
                new_src_ip,
                new_src_port,
                tuple.protocol,
                ipv4_packet_mut,
            );

            return;
        }

        let mut snat_guard = tracker.snat.lock();
        if let Some((new_src_ip, new_src_port)) = snat_guard.process_new_connection(tuple) {
            // log::info!(
            //     "SNAT: Translating {}:{} -> {}:{}",
            //     tuple.src_addr,
            //     tuple.src_port,
            //     new_src_ip,
            //     new_src_port
            // );

            //TODO 应该加一个判断snat，可以支持直接改成出口接口的ip
            // // 修改源IP地址
            SnatPolicy::update_src(
                tuple.dst_addr,
                new_src_ip,
                new_src_port,
                tuple.protocol,
                ipv4_packet_mut,
            );

            return;
        }
    }

    /// 检查IP地址是否是当前接口的IP
    /// todo 这里实现有误，不应该判断是否当前接口的IP，而是应该判断是否是当前网络命名空间的IP
    fn netns_router(&self) -> Arc<Router> {
        self.net_namespace()
            .map_or_else(init_netns_router, |ns| ns.router())
    }
}

fn sub_ttl_ipv4(ipv4_packet: &mut Ipv4Packet<&mut Vec<u8>>) {
    let new_ttl = ipv4_packet.hop_limit().saturating_sub(1);
    ipv4_packet.set_hop_limit(new_ttl);
}

/// # 每一个`RouterEnableDevice`应该有的公共数据，包含
/// - 当前接口的arp_table，记录邻居（//todo：将网卡的发送以及处理逻辑从smoltcp中移动出来，目前只是简单为veth实现这个，因为可以直接查到对端的mac地址）
#[derive(Debug)]
pub struct RouterEnableDeviceCommon {
    /// 当前接口的邻居缓存
    // pub arp_table: RwLock<BTreeMap<IpAddress, EthernetAddress>>,
    /// 当前接口的IP地址列表（因为如果直接通过smoltcp获取ip的话可能导致死锁，因此则这里维护一份）
    pub ip_addrs: RwSem<Vec<IpCidr>>,
}

impl Default for RouterEnableDeviceCommon {
    fn default() -> Self {
        Self {
            // arp_table: RwLock::new(BTreeMap::new()),
            ip_addrs: RwSem::new(Vec::new()),
        }
    }
}
