use crate::driver::net::Iface;
use crate::libs::rwlock::RwLock;
use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use smoltcp::wire::{EthernetAddress, EthernetFrame, IpAddress, IpCidr, Ipv4Packet};

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
    /// 路由表 //todo 后面再优化LC-trie，现在先简单用一个Vec，并且应该在这上面加锁(maybe rwlock?) and 指针反而可以不加锁，在这个路由表这里加就行
    route_table: RwLock<Vec<RouteEntry>>,
}

impl Router {
    pub fn new(name: String) -> Self {
        Self {
            name,
            route_table: RwLock::new(Vec::new()),
        }
    }

    pub fn add_route(&mut self, route: RouteEntry) {
        let mut guard = self.route_table.write();
        let pos = guard
            .iter()
            .position(|r| r.metric > route.metric)
            .unwrap_or(guard.len());

        guard.insert(pos, route);
        log::info!("Router {}: Added route to routing table", self.name);
    }

    pub fn remove_route(&mut self, destination: IpCidr) {
        self.route_table
            .write()
            .retain(|route| route.destination != destination);
    }

    pub fn lookup_route(&self, dest_ip: IpAddress) -> Option<RouteDecision> {
        let guard = self.route_table.read();
        // 按最长前缀匹配原则查找路由
        let best = guard
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
            .retain(|route| route.interface.strong_count() > 0);
    }
}

lazy_static! {
    pub static ref GLOBAL_ROUTER: Arc<Router> = Arc::new(Router::new("global_router".to_string()));
}

pub fn global_router() -> Arc<Router> {
    GLOBAL_ROUTER.clone()
}

/// 可供路由设备应该实现的 trait
pub trait RouterEnableDevice: Iface {
    //todo 这里可以直接传一个IpPacket进来？如果目前只有ipv4的话
    fn handle_routable_packet(&self, packet: &[u8]) {
        if packet.len() < 14 {
            return;
        }

        let ether_frame = match EthernetFrame::new_checked(packet) {
            Ok(f) => f,
            Err(_) => return,
        };

        // 只处理IP包(IPv4)
        if ether_frame.ethertype() != smoltcp::wire::EthernetProtocol::Ipv4 {
            return;
        }

        let ipv4_packet = match Ipv4Packet::new_checked(ether_frame.payload()) {
            Ok(p) => p,
            Err(_) => return,
        };

        let dst_ip = ipv4_packet.dst_addr();

        // 检查TTL
        if ipv4_packet.hop_limit() <= 1 {
            log::warn!("TTL exceeded for packet to {}", dst_ip);
            return;
        }

        // 检查是否是发给自己的包（目标IP是否是自己的IP）
        if self.is_my_ip(dst_ip.into()) {
            // 交给本地协议栈处理
            log::info!("Packet destined for local interface {}", self.iface_name());
            //todo
            return;
        }

        // 查询全局路由表//todo 加入namespace之后在这里改成每个设备所属命名空间的Router即可
        let router = global_router();

        let decision = match router.lookup_route(dst_ip.into()) {
            Some(d) => d,
            None => {
                log::warn!("No route to {}", dst_ip);
                return;
            }
        };

        drop(router);

        // 检查是否是从同一个接口进来又要从同一个接口出去（避免回路）
        if self.iface_name() == decision.interface.iface_name() {
            log::warn!("Avoiding routing loop for packet to {}", dst_ip);
            return;
        }

        // 创建修改后的IP包（递减TTL）
        let modified_ip_packet = ether_frame.payload().to_vec();
        // if modified_ip_packet.len() >= 9 {
        //     modified_ip_packet[8] = modified_ip_packet[8].saturating_sub(1);
        //     //todo 这里应该重新计算IP校验和，为了简化先跳过
        // }

        // 交给出接口进行发送
        decision
            .interface
            .route_and_send(decision.next_hop, &modified_ip_packet);

        log::info!(
            "Routed packet from {} to {} via interface {}",
            self.iface_name(),
            dst_ip,
            decision.interface.iface_name()
        );
    }

    /// 路由器决定通过此接口发送包时调用此方法
    /// 同Linux的ndo_start_xmit()
    ///
    /// todo 在这里查询arp_table，找到目标IP对应的mac地址然后拼接，如果找不到的话就需要主动发送arp请求去查询mac地址了，手伸不到smoltcp内部:(
    fn route_and_send(&self, next_hop: IpAddress, ip_packet: &[u8]);

    /// 检查IP地址是否是当前接口的IP
    fn is_my_ip(&self, ip: IpAddress) -> bool;
}

/// # 每一个`RouterEnableDevice`应该有的公共数据，包含
/// - 当前接口的arp_table，记录邻居（//todo：将网卡的发送以及处理逻辑从smoltcp中移动出来，目前只是简单为veth实现这个，因为可以直接查到对端的mac地址）
/// - 当前接口的路由器 （//todo：引入命名空间之后在这里指向当前所属命名空间的Router）
#[derive(Debug)]
pub struct RouterEnableDeviceCommon {
    pub arp_table: RwLock<BTreeMap<IpAddress, EthernetAddress>>,
    pub router: Weak<Router>,
}

impl Default for RouterEnableDeviceCommon {
    fn default() -> Self {
        let router = global_router();
        Self {
            arp_table: RwLock::new(BTreeMap::new()),
            router: Arc::downgrade(&router),
        }
    }
}
