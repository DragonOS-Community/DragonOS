//! ARP邻居信息管理模块
//!
//! 提供获取ARP缓存信息的API，供procfs等模块调用

use alloc::{string::String, vec::Vec};
use smoltcp::wire::{HardwareAddress, IpAddress};

use crate::net::routing::uapi::arp::{ArpFlags, ArpHrd};

/// ARP条目信息
#[derive(Debug, Clone)]
pub struct ArpEntry {
    /// IP地址
    pub ip_addr: IpAddress,
    /// 硬件类型
    pub hw_type: ArpHrd,
    /// 标志位
    pub flags: ArpFlags,
    /// MAC地址
    pub hw_addr: HardwareAddress,
    /// 网络接口名称
    pub device: String,
}

/// 获取当前网络命名空间的所有ARP条目
///
/// # Returns
/// 返回所有网络设备的有效ARP缓存条目
pub fn get_arp_entries() -> Vec<ArpEntry> {
    use crate::process::ProcessManager;
    let mut entries = Vec::new();

    // 获取当前网络命名空间
    let netns = ProcessManager::current_netns();

    // 遍历所有网络设备
    for (_nic_id, iface) in netns.device_list().iter() {
        // 获取设备名称
        let dev_name = iface.iface_name();

        // 获取smoltcp interface
        let smol_iface = iface.smol_iface().lock();
        let inner = &smol_iface.inner;

        // 遍历neighbor cache

        let timestamp = inner.now();
        let cache = inner.neighbor_cache();
        for (ip_addr, neighbor) in cache.iter() {
            // 只显示有效的（未过期的）条目
            if timestamp >= neighbor.expires_at {
                continue;
            }
            // 只处理IPv4地址
            if let IpAddress::Ipv4(ipv4) = ip_addr {
                entries.push(ArpEntry {
                    ip_addr: IpAddress::Ipv4(*ipv4),
                    hw_type: ArpHrd::Ethernet,
                    flags: ArpFlags::COM,
                    hw_addr: neighbor.hardware_addr,
                    device: dev_name.clone(),
                });
            }
        }
    }

    entries
}
