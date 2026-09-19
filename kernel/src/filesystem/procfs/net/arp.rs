//! /proc/net/arp - ARP 缓存表
//!
//! Linux 6.6: net/ipv4/arp.c
//! 输出格式：
//! IP address       HW type     Flags       HW address            Mask     Device
//! <ip>             0x<hatype>  0x<flags>   <mac>                 *        <dev>

use crate::filesystem::{
    procfs::{
        template::{Builder, FileOps, ProcFileBuilder},
        utils::proc_read_seq,
    },
    vfs::{FilePrivateData, IndexNode, InodeMode},
};
use crate::libs::mutex::MutexGuard;
use crate::net::neighbor::{self, ArpEntry};
use crate::process::namespace::net_namespace::NetNamespace;
use crate::process::ProcessManager;
use alloc::string::ToString;
use alloc::{string::String, sync::Arc, sync::Weak, vec::Vec};
use system_error::SystemError;

/// Header row, byte for byte the `seq_puts()` of Linux `arp_seq_show()`.
const ARP_HEADER: &[u8] =
    b"IP address       HW type     Flags       HW address            Mask     Device\n";

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
}

/// Formats one row the way Linux `arp_format_neigh_entry()` does.
fn format_arp_entry(entry: &ArpEntry) -> String {
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

    format!(
        "{:<16} 0x{:<10x}0x{:<10x}{:<17}     *        {}\n",
        ip_str,
        entry.hw_type.as_u16(),
        entry.flags.bits(),
        hw_addr_str,
        entry.device,
    )
}

/// Renders the ARP table from `cursor` on, the way Linux `arp_seq_ops`
/// (`net/ipv4/arp.c`) walks it through `neigh_seq_start()`/`neigh_seq_next()`,
/// and stops once `want` bytes are out.
///
/// `cursor` is the index of the next entry to render, which is the position
/// `neigh_seq_start()` keeps in `*pos`. It is `None` for the first slice of a
/// record, which also emits the header. Returns the index the next slice
/// resumes at, or `None` when the table ends here.
///
/// One slice per call is what keeps an fd from holding a copy of the whole
/// table, the way `seq_file` holds one block: the neighbour table has no
/// capacity limit, so a process that accumulates neighbours must not be able to
/// multiply it by its descriptor count.
///
/// `netns` is the namespace `open()` pinned, not the reader's current one: the
/// walk has to keep reading the table it started on, the way `seq_open_net()`
/// keeps `seq_net_private.net` for the whole life of the fd.
fn render_arp_slice(
    netns: &Arc<NetNamespace>,
    cursor: Option<usize>,
    want: usize,
    out: &mut Vec<u8>,
) -> Option<usize> {
    let entries = neighbor::get_arp_entries(netns);
    let mut index = cursor.unwrap_or(0);
    if cursor.is_none() {
        out.extend_from_slice(ARP_HEADER);
    }

    while index < entries.len() {
        let line = format_arp_entry(&entries[index]);
        // The first row of a slice is always admitted, the way `seq_file` grows
        // its buffer for a record that does not fit: a reader that asked for
        // fewer bytes than one row still advances.
        if !out.is_empty() && out.len() + line.len() > want {
            break;
        }
        out.extend_from_slice(line.as_bytes());
        index += 1;
    }

    if index < entries.len() {
        Some(index)
    } else {
        None
    }
}

impl FileOps for ArpFileOps {
    fn open(&self, data: &mut MutexGuard<FilePrivateData>) -> Result<(), SystemError> {
        // Linux `seq_open_net()` pins `get_proc_net(inode)` in the seq private
        // data when the file is opened, so a `setns()` afterwards cannot make
        // one fd report two tables.
        let FilePrivateData::Procfs(pdata) = &mut **data else {
            return Err(SystemError::EINVAL);
        };
        pdata.net_ns = Some(ProcessManager::current_netns());
        Ok(())
    }

    fn read_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        mut data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        // `seq_file` (`proc_create_net("arp", 0444, net->proc_net,
        // &arp_seq_ops, ...)`, `net/ipv4/arp.c`): one fd holds the block it has
        // not drained yet, never the whole table.
        let netns = {
            let FilePrivateData::Procfs(pdata) = &*data else {
                return Err(SystemError::EINVAL);
            };
            pdata.net_ns.clone().ok_or(SystemError::EINVAL)?
        };
        proc_read_seq(offset, len, buf, &mut data, |cursor, want, out| {
            Ok(render_arp_slice(&netns, cursor, want, out))
        })
    }
}
