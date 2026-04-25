use crate::filesystem::{
    procfs::{
        template::{Builder, DirOps, FileOps, ProcDir, ProcDirBuilder, ProcFileBuilder},
        utils::proc_read,
    },
    vfs::{FilePrivateData, IndexNode, InodeMode},
};
use crate::libs::mutex::MutexGuard;
use crate::net::socket::inet::common::port::PortManager;
use alloc::{
    format,
    string::{String, ToString},
    sync::{Arc, Weak},
    vec::Vec,
};
use system_error::SystemError;

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
        if name == "ip_local_port_range" {
            let mut cached_children = dir.cached_children().write();
            if let Some(child) = cached_children.get(name) {
                return Ok(child.clone());
            }

            let inode = IpLocalPortRangeFileOps::new_inode(dir.self_ref_weak().clone());
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
