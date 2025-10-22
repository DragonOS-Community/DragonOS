use core::marker::PhantomData;

use crate::libs::spinlock::SpinLock;
use crate::time::Duration;
use crate::time::Instant;
use alloc::fmt::Debug;
use alloc::vec::Vec;
use hashbrown::HashMap;
use smoltcp::wire::{IpAddress, IpCidr, Ipv4Packet};

pub(super) trait NatMapping: Debug + Clone + Copy {
    fn last_seen(&self) -> Instant;
    fn update_last_seen(&mut self, time: Instant);
}

impl NatMapping for SnatMapping {
    fn last_seen(&self) -> Instant {
        self.last_seen
    }
    fn update_last_seen(&mut self, now: Instant) {
        self.last_seen = now;
    }
}

impl NatMapping for DnatMapping {
    fn last_seen(&self) -> Instant {
        self.last_seen
    }
    fn update_last_seen(&mut self, now: Instant) {
        self.last_seen = now;
    }
}

pub(super) trait NatPolicy {
    type Rule: Debug + Clone;
    type Mapping: NatMapping + Send + Sync;

    fn translate(rule: &Self::Rule, original: &FiveTuple) -> (FiveTuple, Self::Mapping);
    fn find_matching_rule(rules: &[Self::Rule], tuple: &FiveTuple) -> Option<Self::Rule>;
    fn get_translation_for_return_traffic(mapping: &Self::Mapping) -> (IpAddress, u16);

    fn update_src(
        dst_ip: IpAddress,
        new_src_ip: IpAddress,
        new_src_port: u16,
        protocol: Protocol,
        ipv4_packet_mut: &mut Ipv4Packet<&mut Vec<u8>>,
    ) {
        // 修改源IP地址
        let IpAddress::Ipv4(new_src_ip_v4) = new_src_ip else {
            return;
        };
        ipv4_packet_mut.set_src_addr(new_src_ip_v4);

        let payload_mut = ipv4_packet_mut.payload_mut();
        match protocol {
            Protocol::Tcp => {
                let mut tcp_packet = smoltcp::wire::TcpPacket::new_checked(payload_mut).unwrap();
                tcp_packet.set_src_port(new_src_port);
                // 重新计算TCP校验和
                tcp_packet.fill_checksum(&new_src_ip, &dst_ip);
            }
            Protocol::Udp => {
                let mut udp_packet = smoltcp::wire::UdpPacket::new_checked(payload_mut).unwrap();
                udp_packet.set_src_port(new_src_port);
                // 重新计算UDP校验和
                udp_packet.fill_checksum(&new_src_ip, &dst_ip);
            }
            _ => {}
        }
    }

    fn update_dst(
        src_ip: IpAddress,
        new_dst_ip: IpAddress,
        new_dst_port: u16,
        protocol: Protocol,
        ipv4_packet_mut: &mut Ipv4Packet<&mut Vec<u8>>,
    ) {
        let IpAddress::Ipv4(new_dst_ip_v4) = new_dst_ip else {
            return;
        };
        ipv4_packet_mut.set_dst_addr(new_dst_ip_v4);

        let payload_mut = ipv4_packet_mut.payload_mut();
        match protocol {
            Protocol::Tcp => {
                let mut tcp_packet = smoltcp::wire::TcpPacket::new_checked(payload_mut).unwrap();
                tcp_packet.set_dst_port(new_dst_port);
                // 重新计算TCP校验和
                tcp_packet.fill_checksum(&src_ip, &new_dst_ip);
            }
            Protocol::Udp => {
                let mut udp_packet = smoltcp::wire::UdpPacket::new_checked(payload_mut).unwrap();
                udp_packet.set_dst_port(new_dst_port);
                // 重新计算UDP校验和
                udp_packet.fill_checksum(&src_ip, &new_dst_ip);
            }
            _ => {}
        }
    }
}

#[derive(Debug)]
pub(super) struct NatTracker<P: NatPolicy> {
    rules: Vec<P::Rule>,
    mappings: HashMap<FiveTuple, P::Mapping>,
    reverse_mappings: HashMap<FiveTuple, FiveTuple>,
    policy_marker: PhantomData<P>,
}

impl<P: NatPolicy> Default for NatTracker<P> {
    fn default() -> Self {
        Self {
            rules: Vec::new(),
            mappings: HashMap::new(),
            reverse_mappings: HashMap::new(),
            policy_marker: PhantomData,
        }
    }
}

impl<P: NatPolicy> NatTracker<P> {
    pub fn update_rules(&mut self, rules: Vec<P::Rule>) {
        self.rules = rules;
    }

    pub fn cleanup_expired(&mut self, now: Instant) {
        // 收集需要移除的键
        let expired_keys: Vec<FiveTuple> = self
            .mappings
            .iter()
            .filter(|(_, mapping)| {
                now.duration_since(mapping.last_seen()).unwrap() > Duration::from_secs(300)
            })
            .map(|(key, _)| *key)
            .collect();

        for key in expired_keys {
            self.mappings.remove(&key);
            // 注意：反向映射表的清理比较复杂，因为多个主映射可能共享一个反向键（如果实现端口复用）
            // 或者需要遍历 reverse_mappings 找到 value 为 key 的条目并删除。
            // 为简单起见，这里只清理主表。更健壮的实现需要双向链接或引用计数。
            log::info!("Cleaned up expired connection for key: {:?}", key);
        }
    }

    #[allow(unused)]
    pub fn insert_rule(&mut self, rule: P::Rule) {
        self.rules.push(rule);
    }

    pub fn process_new_connection(&mut self, tuple: &FiveTuple) -> Option<(IpAddress, u16)> {
        // let rules = self.rules.lock();
        let matching_rule = P::find_matching_rule(&self.rules, tuple)?;

        let (translated_tuple, new_mapping) = P::translate(&matching_rule, tuple);

        self.mappings.insert(*tuple, new_mapping);
        self.reverse_mappings
            .insert(translated_tuple.reverse(), *tuple);

        // log::info!(
        //     "Created new NAT mapping. Original: {:?}, Translated: {:?}",
        //     tuple,
        //     translated_tuple
        // );

        // 返回转换后的地址和端口信息，用于修改数据包
        // 注意：这里需要区分是修改源地址还是目的地址，取决于调用者 (SNAT vs DNAT)
        // SNAT返回新的src_ip/port, DNAT返回新的dst_ip/port.
        // `translated_tuple` 包含了所有信息，我们返回对应的部分。
        if translated_tuple.src_addr != tuple.src_addr {
            Some((translated_tuple.src_addr, translated_tuple.src_port))
        } else {
            Some((translated_tuple.dst_addr, translated_tuple.dst_port))
        }
    }

    /// 处理返回流量
    pub fn process_return_traffic(&mut self, tuple: &FiveTuple) -> Option<(IpAddress, u16)> {
        if let Some(original_key) = self.reverse_mappings.get(tuple) {
            if let Some(mapping) = self.mappings.get_mut(original_key) {
                mapping.update_last_seen(Instant::now());
                return Some(P::get_translation_for_return_traffic(mapping));
            }
        }
        None
    }
}

#[derive(Debug, Clone, Copy)]
pub struct SnatPolicy;

impl NatPolicy for SnatPolicy {
    type Rule = SnatRule;
    type Mapping = SnatMapping;

    fn find_matching_rule(rules: &[Self::Rule], tuple: &FiveTuple) -> Option<Self::Rule> {
        rules
            .iter()
            .find(|rule| rule.source_cidr.contains_addr(&tuple.src_addr))
            .cloned()
    }

    fn translate(rule: &Self::Rule, original_tuple: &FiveTuple) -> (FiveTuple, Self::Mapping) {
        let translated_tuple = FiveTuple {
            src_addr: rule.nat_ip,
            src_port: original_tuple.src_port, // 简化端口处理
            ..*original_tuple
        };

        let mapping = SnatMapping {
            original: *original_tuple,
            _translated: translated_tuple,
            last_seen: Instant::now(),
        };

        (translated_tuple, mapping)
    }

    fn get_translation_for_return_traffic(mapping: &Self::Mapping) -> (IpAddress, u16) {
        // 返回流量需要修改目的地址为原始客户端地址
        (mapping.original.src_addr, mapping.original.src_port)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct DnatPolicy;

impl NatPolicy for DnatPolicy {
    type Rule = DnatRule;
    type Mapping = DnatMapping;

    fn find_matching_rule(rules: &[Self::Rule], tuple: &FiveTuple) -> Option<Self::Rule> {
        rules
            .iter()
            .find(|rule| {
                if tuple.dst_addr != rule.external_addr {
                    return false;
                }

                match rule.external_port {
                    Some(port) => tuple.dst_port == port,
                    None => true,
                }
            })
            .cloned()
    }

    fn translate(rule: &Self::Rule, original: &FiveTuple) -> (FiveTuple, Self::Mapping) {
        let new_internal_port = match rule.internal_port {
            Some(port) => port,
            None => original.dst_port,
        };

        let translated_tuple = FiveTuple {
            dst_addr: rule.internal_addr,
            dst_port: new_internal_port,
            ..*original
        };

        let mapping = DnatMapping {
            from_client: *original,
            _to_server: translated_tuple,
            last_seen: Instant::now(),
        };

        (translated_tuple, mapping)
    }

    fn get_translation_for_return_traffic(mapping: &Self::Mapping) -> (IpAddress, u16) {
        // 返回流量需要修改源地址为原始客户端地址
        (mapping.from_client.dst_addr, mapping.from_client.dst_port)
    }
}

#[derive(Debug)]
pub struct ConnTracker {
    pub(super) snat: SpinLock<NatTracker<SnatPolicy>>,
    pub(super) dnat: SpinLock<NatTracker<DnatPolicy>>,
}

impl ConnTracker {
    pub fn cleanup_expired(&self, now: Instant) {
        self.snat.lock().cleanup_expired(now);
        self.dnat.lock().cleanup_expired(now);
    }

    pub fn update_snat_rules(&self, rules: Vec<SnatRule>) {
        self.snat.lock().update_rules(rules);
    }

    pub fn update_dnat_rules(&self, rules: Vec<DnatRule>) {
        self.dnat.lock().update_rules(rules);
    }
}

impl Default for ConnTracker {
    fn default() -> Self {
        Self {
            snat: SpinLock::new(NatTracker::<SnatPolicy>::default()),
            dnat: SpinLock::new(NatTracker::<DnatPolicy>::default()),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum NatPktStatus {
    Untouched,
    ReverseSnat(FiveTuple),
    NewDnat(FiveTuple),
}

// SNAT 规则：匹配来自某个源地址段的流量，并将其转换为指定的公网IP
#[derive(Debug, Clone)]
pub struct SnatRule {
    pub source_cidr: IpCidr,
    pub nat_ip: IpAddress,
}

/// SNAT 映射：记录一个连接的原始元组和转换后的元组
#[derive(Debug, Clone, Copy)]
pub struct SnatMapping {
    pub original: FiveTuple,
    pub _translated: FiveTuple,
    pub last_seen: Instant,
}

#[derive(Debug, Clone)]
pub struct DnatRule {
    pub external_addr: IpAddress,
    pub external_port: Option<u16>,
    pub internal_addr: IpAddress,
    pub internal_port: Option<u16>,
}

#[derive(Debug, Clone, Copy)]
pub struct DnatMapping {
    // The original tuple from the external client's perspective
    pub from_client: FiveTuple,
    // The tuple after DNAT, as seen by the internal server
    pub _to_server: FiveTuple,
    pub last_seen: Instant,
}

/// 五元组结构体，用于唯一标识一个网络连接
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct FiveTuple {
    pub src_addr: IpAddress,
    pub dst_addr: IpAddress,
    pub src_port: u16,
    pub dst_port: u16,
    pub protocol: Protocol,
}

impl FiveTuple {
    pub fn extract_from_ipv4_packet(packet: &Ipv4Packet<&[u8]>) -> Option<FiveTuple> {
        let src_addr = packet.src_addr().into();
        let dst_addr = packet.dst_addr().into();
        let protocol = Protocol::from_bits_truncate(packet.next_header().into());

        match protocol {
            Protocol::Tcp => {
                let tcp_packet = smoltcp::wire::TcpPacket::new_checked(packet.payload()).ok()?;
                Some(FiveTuple {
                    protocol,
                    src_addr,
                    src_port: tcp_packet.src_port(),
                    dst_addr,
                    dst_port: tcp_packet.dst_port(),
                })
            }
            Protocol::Udp => {
                let udp_packet = smoltcp::wire::UdpPacket::new_checked(packet.payload()).ok()?;
                Some(FiveTuple {
                    protocol,
                    src_addr,
                    src_port: udp_packet.src_port(),
                    dst_addr,
                    dst_port: udp_packet.dst_port(),
                })
            }
            _ => None,
        }
    }

    pub fn reverse(&self) -> Self {
        Self {
            src_addr: self.dst_addr,
            dst_addr: self.src_addr,
            src_port: self.dst_port,
            dst_port: self.src_port,
            protocol: self.protocol,
        }
    }
}

bitflags! {
    pub struct Protocol: u8 {
        const HopByHop  = 0x00;
        const Icmp      = 0x01;
        const Igmp      = 0x02;
        const Tcp       = 0x06;
        const Udp       = 0x11;
        const Ipv6Route = 0x2b;
        const Ipv6Frag  = 0x2c;
        const IpSecEsp  = 0x32;
        const IpSecAh   = 0x33;
        const Icmpv6    = 0x3a;
        const Ipv6NoNxt = 0x3b;
        const Ipv6Opts  = 0x3c;
    }
}
