//! Linux's legacy IPv4 main-table view of the namespace FIB.

use core::fmt::{self, Write};

use alloc::sync::{Arc, Weak};
use smoltcp::wire::{IpAddress, IpCidr};
use system_error::SystemError;

use crate::{
    arch::MMArch,
    filesystem::{
        procfs::{
            template::{configure_seq_file, Builder, FileOps, ProcFileBuilder, ProcfsSeekKind},
            utils::{proc_seq_operation, ProcfsSeqOperation},
        },
        vfs::{file::FileFlags, FilePrivateData, IndexNode, InodeMode, OpenFileBehavior},
    },
    libs::mutex::MutexGuard,
    mm::MemoryManagementArch,
    net::route::{self, RouteEntry},
    process::{namespace::net_namespace::NetNamespace, ProcessManager},
};

const HEADER: &[u8] =
    b"Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT";
const LINE_WIDTH: usize = 127;
const RECORD_SIZE: usize = LINE_WIDTH + 1;
const RTF_UP: u16 = 0x0001;
const RTF_GATEWAY: u16 = 0x0002;
const RTF_HOST: u16 = 0x0004;

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

/// A Linux route record is padded in bytes, including with UTF-8 device names.
struct LineWriter<'a>(&'a mut [u8]);

impl Write for LineWriter<'_> {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        let (field, remaining) = core::mem::take(&mut self.0)
            .split_at_mut_checked(text.len())
            .ok_or(fmt::Error)?;
        field.copy_from_slice(text.as_bytes());
        self.0 = remaining;
        Ok(())
    }
}

fn write_entry(
    line: &mut [u8],
    netns: &NetNamespace,
    entry: &RouteEntry,
) -> Result<(), SystemError> {
    let IpCidr::Ipv4(destination) = entry.destination else {
        return Err(SystemError::EINVAL);
    };
    // Linux prints the native integer representation of the network-order bytes.
    let address = u32::from_ne_bytes(destination.address().octets());
    let gateway = match entry.gateway {
        Some(IpAddress::Ipv4(gateway)) => u32::from_ne_bytes(gateway.octets()),
        _ => 0,
    };
    let prefix = destination.prefix_len();
    let netmask = u32::MAX.checked_shl(u32::from(32 - prefix)).unwrap_or(0);
    let mask = u32::from_ne_bytes(netmask.to_be_bytes());
    let mut flags = RTF_UP;
    if gateway != 0 {
        flags |= RTF_GATEWAY;
    }
    if prefix == 32 {
        flags |= RTF_HOST;
    }

    let iface = netns.device_list().get(&(entry.oif as usize)).cloned();
    let mut write_line = |name: &str| {
        write!(
            LineWriter(line),
            "{name}\t{address:08X}\t{gateway:08X}\t{flags:04X}\t0\t0\t{}\t{mask:08X}\t0\t0\t0",
            entry.priority as i32,
        )
    };
    match iface {
        Some(iface) => iface.common().with_iface_name(write_line),
        None => write_line("*"),
    }
    .map_err(|_| SystemError::EOVERFLOW)
}

impl FileOps for RouteFileOps {
    const SEEK_KIND: ProcfsSeekKind = ProcfsSeekKind::Seq;

    fn configure_open_file(&self, _data: &FilePrivateData, behavior: &mut OpenFileBehavior) {
        configure_seq_file(behavior);
    }

    fn supports_read_iter_flags(&self) -> bool {
        false
    }

    fn open(
        &self,
        data: &mut MutexGuard<FilePrivateData>,
        flags: &FileFlags,
    ) -> Result<(), SystemError> {
        if flags.access_flags() != FileFlags::O_RDONLY {
            return Err(SystemError::EACCES);
        }
        let FilePrivateData::Procfs(pdata) = &mut **data else {
            return Err(SystemError::EINVAL);
        };
        // /proc/net belongs to the opener's netns in DragonOS. Keep that view
        // alive across setns(), unshare() and descriptor transfer.
        pdata.net_ns = Some(ProcessManager::current_netns());
        Ok(())
    }

    fn seq_operation(
        &self,
        operation: ProcfsSeqOperation<'_>,
        data: &mut MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        let FilePrivateData::Procfs(pdata) = &**data else {
            return Err(SystemError::EINVAL);
        };
        let netns = pdata.net_ns.clone().ok_or(SystemError::EINVAL)?;
        let seek = matches!(operation, ProcfsSeqOperation::Seek { .. });
        // At most one compact snapshot per syscall, even when seeking through
        // many records. Only the unfinished text slice survives in the fd.
        let mut snapshot = None;
        proc_seq_operation(operation, data, |cursor, want, out| {
            let Some(index) = cursor else {
                out.try_reserve_exact(RECORD_SIZE)
                    .map_err(|_| SystemError::ENOMEM)?;
                out.extend_from_slice(HEADER);
                out.resize(LINE_WIDTH, b' ');
                out.push(b'\n');
                return Ok(Some(0));
            };
            let entries = match &mut snapshot {
                Some(entries) => entries,
                slot => slot.insert(route::snapshot_ipv4_main(&netns)?),
            };
            // A regular read caches one page even for a single-byte request.
            // Seek traversal still stops at the record containing its offset.
            let bytes = if seek { want } else { MMArch::PAGE_SIZE };
            let count = (bytes / RECORD_SIZE)
                .max(1)
                .min(entries.len().saturating_sub(index));
            if count == 0 {
                return Ok(None);
            }
            let end = index + count;
            out.try_reserve_exact(count * RECORD_SIZE)
                .map_err(|_| SystemError::ENOMEM)?;
            out.resize(count * RECORD_SIZE, b' ');
            for (line, row) in out.chunks_exact_mut(RECORD_SIZE).zip(&entries[index..end]) {
                line[LINE_WIDTH] = b'\n';
                write_entry(&mut line[..LINE_WIDTH], &netns, &row.entry)?;
            }
            Ok((end < entries.len()).then_some(end))
        })
    }
}
