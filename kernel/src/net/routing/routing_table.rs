use core::sync::atomic::AtomicU32;

use crate::time::Instant;
use alloc::vec::Vec;
use smoltcp::wire::{IpAddress, IpCidr};

static DEFAULT_TABLE_ID: AtomicU32 = AtomicU32::new(0);

fn generate_table_id() -> u32 {
    DEFAULT_TABLE_ID.fetch_add(1, core::sync::atomic::Ordering::Relaxed)
}

#[derive(Debug, Clone)]
pub struct NextHop {
    // 出口接口编号
    pub if_index: usize,
    pub via_router: IpAddress,
}

#[derive(Debug, Clone)]
pub struct RouteEntry {
    pub destination: IpCidr,
    pub next_hop: NextHop,

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
    pub fn lookup_route(&self, dest_ip: IpAddress) -> Option<&RouteEntry> {
        self.entries
            .iter()
            .filter(|entry| entry.destination.contains_addr(&dest_ip))
            .max_by_key(|entry| entry.destination.prefix_len()) // 最长前缀匹配
    }
}
