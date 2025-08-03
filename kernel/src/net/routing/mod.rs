pub mod router;
mod routing_table;

// #[derive(Debug)]
// pub struct RouteTable {
//     pub table_id: u32,
//     pub entries: BTreeMap<IpCidr, RouteEntry>,
// }

// impl RouteTable {
//     pub fn new(table_id: u32) -> Self {
//         RouteTable {
//             table_id,
//             entries: BTreeMap::new(),
//         }
//     }

//     pub fn add_route(&mut self, cidr: IpCidr, entry: RouteEntry) {
//         self.entries.insert(cidr, entry);
//     }

//     pub fn del_route(&mut self, cidr: &IpCidr) {
//         self.entries.remove(cidr);
//     }

//     pub fn lookup(&self, ip: &IpAddress, now: Instant) -> Option<&NextHop> {
//         self.entries
//             .iter()
//             .filter(|(cidr, entry)| {
//                 cidr.contains_addr(ip) && entry.expired_at.map_or(true, |t| now <= t)
//             })
//             .max_by_key(|(cidr, _entry)| cidr.prefix_len())
//             .and_then(|(_cidr, entry)| entry.next_hops.first())
//     }
// }

// pub struct RoutingSubsystem {
//     pub route_tables: Vec<RouteTable>,
//     pub rules: Vec<RoutingRule>,
// }

// impl RoutingSubsystem {
//     pub fn new() -> Self {
//         RoutingSubsystem {
//             route_tables: Vec::new(),
//             rules: Vec::new(),
//         }
//     }

//     pub fn get_table_mut(&mut self, table_id: u32) -> Option<&mut RouteTable> {
//         self.route_tables
//             .iter_mut()
//             .find(|t| t.table_id == table_id)
//     }

//     pub fn add_route_table(&mut self, table: RouteTable) {
//         self.route_tables.push(table);
//     }

//     pub fn add_routing_rule(&mut self, rule: RoutingRule) {
//         self.rules.push(rule);
//     }

//     pub fn lookup_route(&self, packet: &PacketMeta) -> Option<&NextHop> {
//         if let Some(rule) = self
//             .rules
//             .iter()
//             .filter(|r| r.matches(packet))
//             .min_by_key(|r| r.priority)
//         {
//             return self
//                 .route_tables
//                 .iter()
//                 .find(|t| t.table_id == rule.table_id)
//                 .and_then(|t| t.lookup(&packet.dst_ip, Instant::now()));
//         }
//         None
//     }
// }

// #[derive(Debug, Clone)]
// pub struct RoutingRule {
//     pub from: Option<IpCidr>,
//     pub tos: Option<u8>,
//     pub fwmark: Option<u32>,
//     pub table_id: u32,
//     // 匹配优先级，数字越小优先匹配
//     pub priority: u32,
// }

// pub struct PacketMeta {
//     pub src_ip: IpAddress,
//     pub dst_ip: IpAddress,
//     pub tos: u8,
//     pub fwmark: u32,
// }

// impl RoutingRule {
//     pub fn matches(&self, packet: &PacketMeta) -> bool {
//         if let Some(ref from) = self.from {
//             if !from.contains_addr(&packet.src_ip) {
//                 return false;
//             }
//         }

//         if let Some(tos) = self.tos {
//             if packet.tos != tos {
//                 return false;
//             }
//         }

//         if let Some(fwmark) = self.fwmark {
//             if packet.fwmark != fwmark {
//                 return false;
//             }
//         }

//         true
//     }
// }

//?test
// pub fn router_probe(name1: &str, name2: &str) -> (Arc<VethInterface>, Arc<VethInterface>) {
//     let (iface1, iface2) = VethInterface::new_pair(name1, name2);

//     let addr1 = IpAddress::v4(10, 0, 0, 1);
//     let cidr1 = IpCidr::new(addr1, 24);
//     iface1.update_ip_addrs(cidr1);

//     let addr2 = IpAddress::v4(10, 0, 0, 2);
//     let cidr2 = IpCidr::new(addr2, 24);
//     iface2.update_ip_addrs(cidr2);

//     // 添加默认路由
//     iface1.add_default_route_to_peer(addr2);
//     iface2.add_default_route_to_peer(addr1);

//     let turn_on = |a: &Arc<VethInterface>| {
//         a.set_net_state(NetDeivceState::__LINK_STATE_START);
//         a.set_operstate(Operstate::IF_OPER_UP);
//         NET_DEVICES.write_irqsave().insert(a.nic_id(), a.clone());
//         register_netdevice(a.clone()).expect("register veth device failed");
//     };

//     turn_on(&iface1);
//     turn_on(&iface2);

//     (iface1, iface2)
// }

// #[unified_init(INITCALL_DEVICE)]
// pub fn veth_init() -> Result<(), SystemError> {
//     router_probe("veth0", "veth1");
//     log::info!("Veth pair initialized.");
//     Ok(())
// }



