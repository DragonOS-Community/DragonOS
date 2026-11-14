//! /proc/net/route - IPv4 主路由表
//!
//! Linux 6.6: `net/ipv4/fib_trie.c` 的 `fib_route_seq_show()`，只导出
//! `RT_TABLE_MAIN` 的 IPv4 路由。输出格式（seq_file，每行补空格到 127 列）：
//!
//! ```text
//! Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT
//! eth0\t00000000\t019018AC\t0003\t0\t0\t0\t00000000\t0\t0\t0
//! ```
//!
//! 其中地址/掩码是网络字节序的 8 位大写十六进制（等价于 `u32::from_le_bytes`）。

use crate::filesystem::procfs::template::configure_seq_file;
use crate::filesystem::vfs::OpenFileBehavior;
use crate::filesystem::{
    procfs::{
        template::{Builder, FileOps, ProcFileBuilder},
        utils::proc_seq_operation,
    },
    vfs::{FilePrivateData, IndexNode, InodeMode},
};
use crate::libs::mutex::MutexGuard;
use crate::net::route::{self, RouteEntry, RT_TABLE_MAIN};
use crate::process::namespace::net_namespace::NetNamespace;
use crate::process::ProcessManager;
use alloc::collections::BTreeMap;
use alloc::{format, string::String, sync::Arc, sync::Weak, vec::Vec};
use smoltcp::wire::{IpCidr, Ipv4Address};
use system_error::SystemError;

/// Header row, byte for byte the `seq_puts()` of Linux `fib_route_seq_show()`.
const ROUTE_HEADER: &[u8] =
    b"Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT";

/// Linux `seq_setwidth(seq, 127)`: every row is padded to 127 columns.
const ROUTE_LINE_WIDTH: usize = 127;

/// Linux `RTF_UP` (include/uapi/linux/route.h)。
const RTF_UP: u16 = 0x0001;
/// Linux `RTF_GATEWAY`。
const RTF_GATEWAY: u16 = 0x0002;
/// Linux `RTF_HOST`：目的地址是单个主机（前缀长度 32）。
const RTF_HOST: u16 = 0x0004;

/// /proc/net/route 文件的 FileOps 实现
#[derive(Debug)]
pub struct RouteFileOps;

impl RouteFileOps {
    pub fn new_inode(parent: Weak<dyn IndexNode>) -> Arc<dyn IndexNode> {
        ProcFileBuilder::new(Self, InodeMode::S_IRUGO)
            .parent(parent)
            .build()
            .unwrap()
    }
}

/// 网络字节序地址按 Linux `%08X` 的形式打印：等价于小端解释同一批字节。
fn format_addr_hex(address: Ipv4Address) -> String {
    format!("{:08X}", u32::from_le_bytes(address.octets()))
}

/// 前缀长度对应的点分掩码，再按 Linux 的十六进制形式打印。
fn format_mask_hex(prefix_len: u8) -> String {
    let prefix_len = prefix_len.min(32);
    let mask = if prefix_len == 0 {
        0u32
    } else {
        u32::MAX << (32 - prefix_len)
    };
    format!("{:08X}", u32::from_le_bytes(mask.to_be_bytes()))
}

/// 由内核路由项推导 Linux 用户态看到的 rt_flags。
///
/// DragonOS 的 `RouteEntry` 没有直接的 flags 字段，这三个位是 Linux 在
/// `fib_route_seq_show()` 里等价表达的信息：主表里能查到的路由都是 UP；
/// 有下一跳就是 GATEWAY；前缀 32 是 HOST 路由。
fn linux_route_flags(entry: &RouteEntry, prefix_len: u8) -> u16 {
    let mut flags = RTF_UP;
    if entry.gateway.is_some() {
        flags |= RTF_GATEWAY;
    }
    if prefix_len == 32 {
        flags |= RTF_HOST;
    }
    flags
}

/// 取当前命名空间的 ifindex -> 接口名映射。
///
/// Linux 的 `fib_route_seq_show()` 打印 `dev->name`，没有设备时打印 `*`。
fn device_names(netns: &Arc<NetNamespace>) -> BTreeMap<u32, String> {
    let mut names = BTreeMap::new();
    for iface in netns.device_list().iter().map(|(_, iface)| iface) {
        names.insert(iface.nic_id() as u32, iface.iface_name());
    }
    names
}

/// Formats one row the way Linux `fib_route_seq_show()` does.
///
/// Returns the sort key Linux's trie walk produces (prefix length, then
/// destination address) together with the rendered row: `fib_route_seq_show()`
/// walks the trie leaves in ascending prefix order, so the default route is
/// printed before more specific ones.
fn format_route_entry(
    entry: &RouteEntry,
    names: &BTreeMap<u32, String>,
) -> Option<((u8, u32), String)> {
    let IpCidr::Ipv4(destination) = entry.destination else {
        // /proc/net/route 只导出 IPv4；IPv6 在 /proc/net/ipv6_route。
        return None;
    };
    if entry.table != RT_TABLE_MAIN {
        // Linux 只遍历主表（RT_TABLE_MAIN），local/default 表不导出。
        return None;
    }

    let iface = names.get(&entry.oif).map(String::as_str).unwrap_or("*");
    let gateway = match entry.gateway {
        Some(smoltcp::wire::IpAddress::Ipv4(gateway)) => format_addr_hex(gateway),
        _ => format!("{:08X}", 0u32),
    };
    let address = destination.address();
    let prefix_len = destination.prefix_len();
    let line = format!(
        "{}\t{}\t{}\t{:04X}\t0\t0\t{}\t{}\t0\t0\t0",
        iface,
        format_addr_hex(address),
        gateway,
        linux_route_flags(entry, prefix_len),
        entry.priority,
        format_mask_hex(prefix_len),
    );
    Some((
        (prefix_len, u32::from_le_bytes(address.octets())),
        format!("{:<width$}", line, width = ROUTE_LINE_WIDTH),
    ))
}

/// Renders the route table from `cursor` on, the way Linux `fib_route_seq_ops`
/// walks it, and stops once `want` bytes are out.
///
/// `cursor` is the index of the next entry to render; it is `None` for the
/// first slice of a record, which also emits the header padded to 127 columns
/// just like `seq_setwidth()` does.
fn render_route_slice(
    netns: &Arc<NetNamespace>,
    cursor: Option<usize>,
    want: usize,
    out: &mut Vec<u8>,
) -> Option<usize> {
    let entries = match route::snapshot(netns) {
        Ok(entries) => entries,
        Err(error) => {
            log::warn!("failed to snapshot routes: {:?}", error);
            return None;
        }
    };
    let names = device_names(netns);

    // 过滤在取设备名之后做，避免把非主表/非 IPv4 的行计入 cursor。
    let mut keyed: Vec<((u8, u32), String)> = entries
        .iter()
        .filter_map(|entry| format_route_entry(entry, &names))
        .collect();
    keyed.sort_by_key(|(key, _)| *key);
    let rows: Vec<String> = keyed.into_iter().map(|(_, line)| line).collect();

    let mut index = cursor.unwrap_or(0);
    if cursor.is_none() {
        out.extend_from_slice(
            format!("{:<width$}\n", core::str::from_utf8(ROUTE_HEADER).unwrap_or(""), width = ROUTE_LINE_WIDTH)
                .as_bytes(),
        );
    }

    while index < rows.len() {
        let line = format!("{}\n", rows[index]);
        if !out.is_empty() && out.len() + line.len() > want {
            break;
        }
        out.extend_from_slice(line.as_bytes());
        index += 1;
    }

    if index < rows.len() {
        Some(index)
    } else {
        None
    }
}

impl FileOps for RouteFileOps {
    fn configure_open_file(&self, _data: &FilePrivateData, behavior: &mut OpenFileBehavior) {
        configure_seq_file(behavior);
    }

    fn supports_read_iter_flags(&self) -> bool {
        false
    }

    fn open(
        &self,
        data: &mut MutexGuard<FilePrivateData>,
        _flags: &crate::filesystem::vfs::file::FileFlags,
    ) -> Result<(), SystemError> {
        // Linux `seq_open_net()`：open() 时把 netns 钉在这个 fd 上，
        // 之后 setns() 不能让同一个 fd 读到两个命名空间的表。
        let FilePrivateData::Procfs(pdata) = &mut **data else {
            return Err(SystemError::EINVAL);
        };
        pdata.net_ns = Some(ProcessManager::current_netns());
        Ok(())
    }

    const SEEK_KIND: crate::filesystem::procfs::template::ProcfsSeekKind =
        crate::filesystem::procfs::template::ProcfsSeekKind::Seq;

    fn seq_operation(
        &self,
        operation: crate::filesystem::procfs::utils::ProcfsSeqOperation<'_>,
        data: &mut MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        let netns = {
            let FilePrivateData::Procfs(pdata) = &**data else {
                return Err(SystemError::EINVAL);
            };
            pdata.net_ns.clone().ok_or(SystemError::EINVAL)?
        };
        proc_seq_operation(operation, data, |cursor, want, out| {
            Ok(render_route_slice(&netns, cursor, want, out))
        })
    }
}
