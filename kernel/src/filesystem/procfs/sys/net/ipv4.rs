use crate::filesystem::{
    procfs::{
        template::{Builder, DirOps, FileOps, ProcDir, ProcDirBuilder, ProcFileBuilder},
        utils::proc_read,
    },
    vfs::{FilePrivateData, IndexNode, InodeMode},
};
use crate::libs::mutex::MutexGuard;
use crate::net::socket::inet::common::port::PortManager;
use crate::process::{cred::CAPFlags, ProcessManager};
use alloc::{
    format,
    string::{String, ToString},
    sync::{Arc, Weak},
    vec::Vec,
};
use system_error::SystemError;

use super::super::numeric::parse_numeric_sysctl;

#[derive(Debug)]
pub struct Ipv4DirOps;

impl Ipv4DirOps {
    pub fn new_inode(parent: Weak<dyn IndexNode>) -> Arc<dyn IndexNode> {
        ProcDirBuilder::new(Self, InodeMode::from_bits_truncate(0o555))
            .parent(parent)
            .build()
            .unwrap()
    }
}

impl DirOps for Ipv4DirOps {
    fn lookup_child(
        &self,
        dir: &ProcDir<Self>,
        name: &str,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        if name == "ip_local_port_range" || name == "ip_forward" {
            let mut cached_children = dir.cached_children().write();
            if let Some(child) = cached_children.get(name) {
                return Ok(child.clone());
            }

            let inode = if name == "ip_forward" {
                IpForwardFileOps::new_inode(dir.self_ref_weak().clone())
            } else {
                IpLocalPortRangeFileOps::new_inode(dir.self_ref_weak().clone())
            };
            cached_children.insert(name.to_string(), inode.clone());
            return Ok(inode);
        }

        Err(SystemError::ENOENT)
    }

    fn populate_children(&self, dir: &ProcDir<Self>) {
        let mut cached_children = dir.cached_children().write();
        cached_children
            .entry("ip_local_port_range".to_string())
            .or_insert_with(|| IpLocalPortRangeFileOps::new_inode(dir.self_ref_weak().clone()));
        cached_children
            .entry("ip_forward".to_string())
            .or_insert_with(|| IpForwardFileOps::new_inode(dir.self_ref_weak().clone()));
    }
}

#[derive(Debug)]
pub struct IpLocalPortRangeFileOps;

impl IpLocalPortRangeFileOps {
    pub fn new_inode(parent: Weak<dyn IndexNode>) -> Arc<dyn IndexNode> {
        ProcFileBuilder::new(Self, InodeMode::from_bits_truncate(0o644))
            .parent(parent)
            .build()
            .unwrap()
    }

    fn read_config() -> String {
        let (min, max) = PortManager::local_port_range();
        format!("{} {}\n", min, max)
    }

    fn write_config(data: &[u8]) -> Result<usize, SystemError> {
        let input = core::str::from_utf8(data).map_err(|_| SystemError::EINVAL)?;
        let parts: Vec<&str> = input.split_whitespace().collect();
        if parts.len() < 2 {
            return Err(SystemError::EINVAL);
        }
        let min: u16 = parts[0].parse().map_err(|_| SystemError::EINVAL)?;
        let max: u16 = parts[1].parse().map_err(|_| SystemError::EINVAL)?;
        PortManager::set_local_port_range(min, max)?;
        Ok(data.len())
    }
}

impl FileOps for IpLocalPortRangeFileOps {
    fn read_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        let content = Self::read_config();
        proc_read(offset, len, buf, content.as_bytes())
    }

    fn write_at(
        &self,
        _offset: usize,
        _len: usize,
        buf: &[u8],
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        Self::write_config(buf)
    }
}

#[derive(Debug)]
struct IpForwardFileOps;

impl IpForwardFileOps {
    fn new_inode(parent: Weak<dyn IndexNode>) -> Arc<dyn IndexNode> {
        ProcFileBuilder::new(Self, InodeMode::from_bits_truncate(0o644))
            .parent(parent)
            .sysctl_permissions()
            .build()
            .unwrap()
    }
}

impl FileOps for IpForwardFileOps {
    fn read_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        if offset != 0 {
            return Ok(0);
        }
        let value = ProcessManager::current_netns().ipv4_forwarding_value();
        proc_read(0, len, buf, format!("{value}\n").as_bytes())
    }

    fn write_at(
        &self,
        offset: usize,
        _len: usize,
        buf: &[u8],
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        let netns = ProcessManager::current_netns();
        if !ProcessManager::current_pcb()
            .cred()
            .has_capability_in_ns(netns.user_ns(), CAPFlags::CAP_NET_ADMIN)
        {
            return Err(SystemError::EPERM);
        }
        if buf.is_empty() {
            return Ok(0);
        }
        if offset != 0 {
            return Ok(buf.len());
        }
        let (value, consumed) = parse_numeric_sysctl(buf)?;
        let value = i32::try_from(value).map_err(|_| SystemError::EINVAL)?;
        netns.set_ipv4_forwarding(value);
        Ok(consumed)
    }
}
