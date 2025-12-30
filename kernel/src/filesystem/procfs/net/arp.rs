//! /proc/net/arp - ARP 缓存表
//!
//! Linux 6.6: net/ipv4/arp.c
//! 输出格式：
//! IP address       HW type     Flags       HW address            Mask     Device
//! <ip>             0x<hatype>  0x<flags>   <mac>                 *        <dev>

use crate::filesystem::{
    procfs::{
        template::{Builder, FileOps, ProcFileBuilder},
        utils::proc_read,
    },
    vfs::{FilePrivateData, IndexNode, InodeMode},
};
use crate::net::neighbor;
use alloc::string::ToString;
use alloc::{string::String, sync::Arc, sync::Weak, vec::Vec};
use system_error::SystemError;

/// /proc/net/arp 文件的 FileOps 实现
#[derive(Debug)]
pub struct ArpFileOps;

impl ArpFileOps {
    pub fn new_inode(parent: Weak<dyn IndexNode>) -> Arc<dyn IndexNode> {
        ProcFileBuilder::new(Self, InodeMode::S_IRUGO)
            .parent(parent)
            .build()
            .unwrap()
    }

    fn generate_arp_content() -> Vec<u8> {
        let mut content = String::from(
            "IP address       HW type     Flags       HW address            Mask     Device\n",
        );

        // 调用网络子系统的API获取ARP条目
        let entries = neighbor::get_arp_entries();

        // 格式化输出每个条目
        for entry in entries {
            // Linux uses %-16s for IPv4 strings (see net/ipv4/arp.c: arp_format_neigh_entry)
            // smoltcp::wire::IpAddress's Display implementation does not honor formatter width,
            // so we stringify first and apply padding to the String.
            let ip_str = entry.ip_addr.to_string();

            // Linux prints MAC as lowercase hex with ':' separators.
            let hw_addr_str = match entry.hw_addr {
                smoltcp::wire::HardwareAddress::Ethernet(eth) => {
                    let b = eth.0;
                    format!(
                        "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
                        b[0], b[1], b[2], b[3], b[4], b[5]
                    )
                }
                _ => entry
                    .hw_addr
                    .to_string()
                    .replace('-', ":")
                    .to_ascii_lowercase(),
            };

            content.push_str(&format!(
                "{:<16} 0x{:<10x}0x{:<10x}{:<17}     *        {}\n",
                ip_str,
                entry.hw_type.as_u16(),
                entry.flags.bits(),
                hw_addr_str,
                entry.device,
            ));
        }

        content.into_bytes()
    }
}

impl FileOps for ArpFileOps {
    fn read_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        _data: crate::libs::spinlock::SpinLockGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        let content = Self::generate_arp_content();
        proc_read(offset, len, buf, &content)
    }
}
