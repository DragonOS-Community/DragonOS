use alloc::sync::Arc;
use core::{net::Ipv4Addr, sync::atomic::AtomicU32};

use crate::{driver::net::route_iface::RouteInterface, time::Instant};
use alloc::vec::Vec;
use smoltcp::wire::{IpAddress, IpCidr};

static DEFAULT_TABLE_ID: AtomicU32 = AtomicU32::new(0);

fn generate_table_id() -> u32 {
    DEFAULT_TABLE_ID.fetch_add(1, core::sync::atomic::Ordering::Relaxed)
}

#[derive(Debug, Clone)]
pub struct RouteEntry {
    pub destination: IpCidr,
    pub next_hop: Option<IpAddress>,
    pub interface: Arc<RouteInterface>,

    // None 表示永久有效
    pub prefer_until: Option<Instant>,
    pub expired_at: Option<Instant>,

    /// 度量值，暂时未用到
    pub metric: u32,
}

#[derive(Debug, Default)]
pub struct RouteTable {
    pub table_id: u32,
    // pub entries: BTreeMap<IpCidr, RouteEntry>,
    entries: Vec<RouteEntry>,
}

impl RouteTable {
    pub fn new() -> Self {
        RouteTable {
            table_id: generate_table_id(),
            entries: Vec::new(),
        }
    }

    pub fn add_route(&mut self, entry: RouteEntry) {
        self.entries.push(entry);
        self.entries
            .sort_by(|a, b| b.destination.prefix_len().cmp(&a.destination.prefix_len()));
    }

    /// 根据目的IP地址查找最佳匹配的路由条目（最长前缀匹配）。
    pub fn lookup_route(&self, dest_ip: Ipv4Addr) -> Option<&RouteEntry> {
        self.entries
            .iter()
            .filter(|entry| entry.destination.contains_addr(&IpAddress::Ipv4(dest_ip)))
            .max_by_key(|entry| entry.destination.prefix_len()) // 最长前缀匹配
    }

    pub fn remove_route(&mut self, cidr: &IpCidr) {
        self.entries.retain(|entry| entry.destination != *cidr);
    }

    pub fn lookup(&self, dest_ip: &IpAddress) -> Option<(Arc<RouteInterface>, Option<IpAddress>)> {
        let mut best_match: Option<(&RouteEntry, u8)> = None;

        for entry in &self.entries {
            if entry.destination.contains_addr(dest_ip) {
                let current_prefix_len = entry.destination.prefix_len();
                if let Some((_, prev_prefix_len)) = best_match {
                    // If a previous match exists, check if the current one is more specific
                    if current_prefix_len > prev_prefix_len {
                        best_match = Some((entry, current_prefix_len));
                    }
                } else {
                    // First match found
                    best_match = Some((entry, current_prefix_len));
                }
            }
        }
        best_match.map(|(entry, _)| (entry.interface.clone(), entry.next_hop))
    }
}
