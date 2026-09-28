//! Namespace-scoped IPv6 forwarding control.

use crate::filesystem::procfs::Builder;
use crate::filesystem::{
    procfs::{
        template::{DirOps, FileOps, ProcDir, ProcDirBuilder, ProcFileBuilder},
        utils::proc_read,
    },
    vfs::{FilePrivateData, IndexNode, InodeMode},
};
use crate::libs::mutex::MutexGuard;
use crate::process::{cred::CAPFlags, ProcessManager};
use alloc::{
    format,
    string::ToString,
    sync::{Arc, Weak},
};
use system_error::SystemError;

use super::super::numeric::parse_numeric_sysctl;

#[derive(Debug)]
pub(super) struct Ipv6DirOps;

#[derive(Debug)]
struct ConfDirOps;

#[derive(Debug)]
struct AllDirOps;

impl Ipv6DirOps {
    pub(super) fn new_inode(parent: Weak<dyn IndexNode>) -> Arc<dyn IndexNode> {
        ProcDirBuilder::new(Self, InodeMode::from_bits_truncate(0o555))
            .parent(parent)
            .build()
            .unwrap()
    }
}

impl ConfDirOps {
    fn new_inode(parent: Weak<dyn IndexNode>) -> Arc<dyn IndexNode> {
        ProcDirBuilder::new(Self, InodeMode::from_bits_truncate(0o555))
            .parent(parent)
            .build()
            .unwrap()
    }
}

impl AllDirOps {
    fn new_inode(parent: Weak<dyn IndexNode>) -> Arc<dyn IndexNode> {
        ProcDirBuilder::new(Self, InodeMode::from_bits_truncate(0o555))
            .parent(parent)
            .build()
            .unwrap()
    }
}

impl DirOps for Ipv6DirOps {
    fn lookup_child(
        &self,
        dir: &ProcDir<Self>,
        name: &str,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        if name != "conf" {
            return Err(SystemError::ENOENT);
        }
        let mut children = dir.cached_children().write();
        Ok(children
            .entry(name.to_string())
            .or_insert_with(|| ConfDirOps::new_inode(dir.self_ref_weak().clone()))
            .clone())
    }

    fn populate_children(&self, dir: &ProcDir<Self>) {
        dir.cached_children()
            .write()
            .entry("conf".to_string())
            .or_insert_with(|| ConfDirOps::new_inode(dir.self_ref_weak().clone()));
    }
}

impl DirOps for ConfDirOps {
    fn lookup_child(
        &self,
        dir: &ProcDir<Self>,
        name: &str,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        if name != "all" {
            return Err(SystemError::ENOENT);
        }
        let mut children = dir.cached_children().write();
        Ok(children
            .entry(name.to_string())
            .or_insert_with(|| AllDirOps::new_inode(dir.self_ref_weak().clone()))
            .clone())
    }

    fn populate_children(&self, dir: &ProcDir<Self>) {
        dir.cached_children()
            .write()
            .entry("all".to_string())
            .or_insert_with(|| AllDirOps::new_inode(dir.self_ref_weak().clone()));
    }
}

impl DirOps for AllDirOps {
    fn lookup_child(
        &self,
        dir: &ProcDir<Self>,
        name: &str,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        if name != "forwarding" {
            return Err(SystemError::ENOENT);
        }
        let mut children = dir.cached_children().write();
        Ok(children
            .entry(name.to_string())
            .or_insert_with(|| ForwardingFileOps::new_inode(dir.self_ref_weak().clone()))
            .clone())
    }

    fn populate_children(&self, dir: &ProcDir<Self>) {
        dir.cached_children()
            .write()
            .entry("forwarding".to_string())
            .or_insert_with(|| ForwardingFileOps::new_inode(dir.self_ref_weak().clone()));
    }
}

#[derive(Debug)]
struct ForwardingFileOps;

impl ForwardingFileOps {
    fn new_inode(parent: Weak<dyn IndexNode>) -> Arc<dyn IndexNode> {
        ProcFileBuilder::new(Self, InodeMode::from_bits_truncate(0o644))
            .parent(parent)
            .sysctl_permissions()
            .build()
            .unwrap()
    }
}

impl FileOps for ForwardingFileOps {
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
        let value = ProcessManager::current_netns().ipv6_forwarding_value();
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
        netns.set_ipv6_forwarding(value);
        Ok(consumed)
    }
}
