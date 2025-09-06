use crate::driver::net::Iface;
use crate::libs::rwlock::RwLock;
use crate::net::routing::nat::ConnTracker;
use crate::net::routing::nat::DnatPolicy;
use crate::net::routing::nat::FiveTuple;
use crate::net::routing::nat::NatPktStatus;
use crate::net::routing::nat::NatPolicy;
use crate::net::routing::nat::SnatPolicy;
use crate::process::namespace::net_namespace::NetNamespace;
use crate::process::namespace::net_namespace::INIT_NET_NAMESPACE;
use alloc::string::{String, ToString};
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::net::Ipv4Addr;
use smoltcp::wire::{EthernetFrame, IpAddress, IpCidr, Ipv4Packet};
use system_error::SystemError;

mod nat;

pub use nat::{DnatRule, SnatRule};

#[derive(Debug, Clone)]
pub struct RouteEntry {
    /// 目标网络
    pub destination: IpCidr,
    /// 下一跳地址（如果是直连网络则为None）
    pub next_hop: Option<IpAddress>,
    /// 出接口
    pub interface: Weak<dyn RouterEnableDevice>,
    /// 路由优先级（数值越小优先级越高）
    pub metric: u32,
    /// 路由类型
    pub route_type: RouteType,
}

#[derive(Debug, Clone, PartialEq)]
pub enum RouteType {
    /// 直连路由
    Connected,
    /// 静态路由
    Static,
    /// 默认路由
    Default,
}

impl RouteEntry {
    pub fn new_connected(destination: IpCidr, interface: Arc<dyn RouterEnableDevice>) -> Self {
        RouteEntry {
            destination,
            next_hop: None,
            interface: Arc::downgrade(&interface),
            metric: 0,
            route_type: RouteType::Connected,
        }
    }

    pub fn new_static(
        destination: IpCidr,
        next_hop: IpAddress,
        interface: Arc<dyn RouterEnableDevice>,
        metric: u32,
    ) -> Self {
        RouteEntry {
            destination,
            next_hop: Some(next_hop),
            interface: Arc::downgrade(&interface),
            metric,
            route_type: RouteType::Static,
        }
    }

    pub fn new_default(next_hop: IpAddress, interface: Arc<dyn RouterEnableDevice>) -> Self {
        RouteEntry {
            destination: IpCidr::new(IpAddress::v4(0, 0, 0, 0), 0),
            next_hop: Some(next_hop),
            interface: Arc::downgrade(&interface),
            metric: 100,
            route_type: RouteType::Default,
        }
    }
}

#[derive(Debug, Default)]
pub struct RouteTable {
    pub entries: Vec<RouteEntry>,
}

/// 路由决策结果
#[derive(Debug)]
pub struct RouteDecision {
    /// 出接口
    pub interface: Arc<dyn RouterEnableDevice>,
    /// 下一跳地址（先写在这里
    pub next_hop: IpAddress,
}

#[derive(Debug)]
pub struct Router {
    name: String,
    /// 路由表 //todo 后面再优化LC-trie，现在先简单用一个Vec
    route_table: RwLock<RouteTable>,
    pub(self) nat_tracker: Arc<ConnTracker>,
    pub ns: RwLock<Weak<NetNamespace>>,
}

impl Router {
    pub fn new(name: String) -> Arc<Self> {
        Arc::new(Self {
            name: name.clone(),
            route_table: RwLock::new(RouteTable::default()),
            nat_tracker: Arc::new(ConnTracker::default()),
            ns: RwLock::new(Weak::default()),
        })
    }

    /// 创建一个空的Router实例，主要用于初始化网络命名空间时使用
    /// 注意： 这个Router实例不会启动轮询线程
    pub fn new_empty() -> Arc<Self> {
        Arc::new(Self {
            name: "empty_router".to_string(),
            route_table: RwLock::new(RouteTable::default()),
            ns: RwLock::new(Weak::default()),
            nat_tracker: Arc::new(ConnTracker::default()),
        })
    }

    pub fn add_route(&self, route: RouteEntry) {
        let mut guard = self.route_table.write();
        let entries = &mut guard.entries;
        let pos = entries
            .iter()
            .position(|r| r.metric > route.metric)
            .unwrap_or(entries.len());

        entries.insert(pos, route);
        log::info!("Router {}: Added route to routing table", self.name);
    }

    pub fn remove_route(&self, destination: IpCidr) {
        self.route_table
            .write()
            .entries
            .retain(|route| route.destination != destination);
    }

    pub fn lookup_route(&self, dest_ip: IpAddress) -> Option<RouteDecision> {
        let guard = self.route_table.read();
        // 按最长前缀匹配原则查找路由
        let best = guard
            .entries
            .iter()
            .filter(|route| {
                route.interface.strong_count() > 0 && route.destination.contains_addr(&dest_ip)
            })
            .max_by_key(|route| route.destination.prefix_len());

        if let Some(entry) = best {
            if let Some(interface) = entry.interface.upgrade() {
                let next_hop = entry.next_hop.unwrap_or(dest_ip);
                return Some(RouteDecision {
                    interface,
                    next_hop,
                });
            }
        }

        None
    }

    /// 清理无效的路由表项（接口已经不存在的）
    pub fn cleanup_routes(&mut self) {
        self.route_table
            .write()
            .entries
            .retain(|route| route.interface.strong_count() > 0);
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

                // 检查TTL
                if ipv4_packet_mut.hop_limit() <= 1 {
                    log::warn!("TTL exceeded for packet to {}", dst_ip);
                    return Err(Some(SystemError::EINVAL));
                }

                // 检查是否是发给自己的包（目标IP是否是自己的IP）
                if self.is_my_ip(dst_ip.into()) {
                    // todo 按照linux的逻辑，只要包的目标ip在当前网络命名空间里面，就直接进入本地协议栈处理
                    // todo 但是我们的操作系统中每个接口都是独立的，并没有统一处理和分发（socket），所有这里必须将包放到对应iface的接收队列里面 
                    // 交给本地协议栈处理
                    // log::info!("Packet destined for local interface {}", self.iface_name());
                    return Err(None);
                }

                // 查询当前网络命名空间下的路由表
                let router = self.netns_router();

                let decision = match router.lookup_route(dst_ip.into()) {
                    Some(d) => d,
                    None => {
                        log::warn!("No route to {}", dst_ip);
                        return Err(None);
                    }
                };

                drop(router);

                // === POST-ROUTING HOOK ===

                let decision_src_ip = decision.interface.common().ipv4_addr().unwrap();
                self.post_routing_hook(
                    &maybe_tuple,
                    &decision_src_ip,
                    &mut ipv4_packet_mut,
                    &pkt_status,
                );
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
                    .route_and_send(next_hop, ipv4_packet_mut.as_ref());

                log::info!("Routed packet from {} to {} ", self.iface_name(), dst_ip);
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
            log::info!(
                "Reverse SNAT: Translating {}:{} to {}:{}",
                tuple.src_addr,
                tuple.src_port,
                new_dst_ip,
                new_dst_port
            );

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
            log::info!(
                "DNAT: Translating {}:{} to {}:{}",
                tuple.dst_addr,
                tuple.dst_port,
                new_dst_ip,
                new_dst_port
            );

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
        _decision_src_ip: &Ipv4Addr,
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
            log::info!(
                "Reverse DNAT: Translating src {}:{} -> {}:{}",
                tuple.src_addr,
                tuple.src_port,
                new_src_ip,
                new_src_port
            );

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
            // let new_src_ip: IpAddress = if let IpAddress::Ipv4(new_src_ip) = new_src_ip {
            //     new_src_ip.into()
            // } else {
            //     (*decision_src_ip).into()
            // };

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

    /// 路由器决定通过此接口发送包时调用此方法
    /// 同Linux的ndo_start_xmit()
    ///
    /// todo 在这里查询arp_table，找到目标IP对应的mac地址然后拼接，如果找不到的话就需要主动发送arp请求去查询mac地址了，手伸不到smoltcp内部:(
    /// 后续需要将arp查询的逻辑从smoltcp中抽离出来
    fn route_and_send(&self, next_hop: &IpAddress, ip_packet: &[u8]);

    /// 检查IP地址是否是当前接口的IP
    /// todo 这里实现有误，不应该判断是否当前接口的IP，而是应该判断是否是当前网络命名空间的IP，然脏
    fn is_my_ip(&self, ip: IpAddress) -> bool;

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
    pub ip_addrs: RwLock<Vec<IpCidr>>,
}

impl Default for RouterEnableDeviceCommon {
    fn default() -> Self {
        Self {
            // arp_table: RwLock::new(BTreeMap::new()),
            ip_addrs: RwLock::new(Vec::new()),
        }
    }
}
