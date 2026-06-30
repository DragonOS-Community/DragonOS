//! /proc/sys/vm - 虚拟内存参数目录

use crate::libs::mutex::MutexGuard;
use crate::{
    filesystem::{
        procfs::template::{Builder, DirOps, FileOps, ProcDir, ProcDirBuilder, ProcFileBuilder},
        vfs::{FilePrivateData, IndexNode, InodeMode},
    },
    mm::{page::PageReclaimer, page_cache_stats},
    process::cred::{capable, CAPFlags},
};
use alloc::{
    string::ToString,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::sync::atomic::{AtomicBool, Ordering};
use system_error::SystemError;

static DROP_CACHES_QUIET: AtomicBool = AtomicBool::new(false);

/// /proc/sys/vm 目录的 DirOps 实现
#[derive(Debug)]
pub struct VmDirOps;

impl VmDirOps {
    pub fn new_inode(parent: Weak<dyn IndexNode>) -> Arc<dyn IndexNode> {
        ProcDirBuilder::new(Self, InodeMode::from_bits_truncate(0o555))
            .parent(parent)
            .build()
            .unwrap()
    }
}

impl DirOps for VmDirOps {
    fn lookup_child(
        &self,
        dir: &ProcDir<Self>,
        name: &str,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        if name == "drop_caches" {
            let mut cached_children = dir.cached_children().write();
            if let Some(child) = cached_children.get(name) {
                return Ok(child.clone());
            }

            let inode = DropCachesFileOps::new_inode(dir.self_ref_weak().clone());
            cached_children.insert(name.to_string(), inode.clone());
            return Ok(inode);
        }
        if name == "oom_fault_inject" {
            let mut cached_children = dir.cached_children().write();
            if let Some(child) = cached_children.get(name) {
                return Ok(child.clone());
            }

            let inode = OomFaultInjectFileOps::new_inode(dir.self_ref_weak().clone());
            cached_children.insert(name.to_string(), inode.clone());
            return Ok(inode);
        }

        Err(SystemError::ENOENT)
    }

    fn populate_children(&self, dir: &ProcDir<Self>) {
        let mut cached_children = dir.cached_children().write();
        cached_children
            .entry("drop_caches".to_string())
            .or_insert_with(|| DropCachesFileOps::new_inode(dir.self_ref_weak().clone()));
        cached_children
            .entry("oom_fault_inject".to_string())
            .or_insert_with(|| OomFaultInjectFileOps::new_inode(dir.self_ref_weak().clone()));
    }
}

/// /proc/sys/vm/drop_caches 文件的 FileOps 实现
#[derive(Debug)]
pub struct DropCachesFileOps;

impl DropCachesFileOps {
    pub fn new_inode(parent: Weak<dyn IndexNode>) -> Arc<dyn IndexNode> {
        ProcFileBuilder::new(Self, InodeMode::from_bits_truncate(0o200))
            .parent(parent)
            .build()
            .unwrap()
    }

    fn write_config(data: &[u8]) -> Result<usize, SystemError> {
        let input = core::str::from_utf8(data).map_err(|_| SystemError::EINVAL)?;
        let parts: Vec<&str> = input.split_whitespace().collect();
        if parts.is_empty() {
            return Err(SystemError::EINVAL);
        }

        let value: u32 = parts[0].parse().map_err(|_| SystemError::EINVAL)?;
        if !(1..=4).contains(&value) {
            return Err(SystemError::EINVAL);
        }

        let quiet = (value & 4) != 0;
        let drop_pagecache = (value & 1) != 0;
        let drop_slab = (value & 2) != 0;

        if drop_pagecache {
            PageReclaimer::drop_pagecache(true);
            page_cache_stats::inc_drop_pagecache();
        }

        if drop_slab {
            log::warn!("drop_caches: drop_slab not supported");
        }

        if quiet {
            DROP_CACHES_QUIET.store(true, Ordering::Relaxed);
        } else if !DROP_CACHES_QUIET.load(Ordering::Relaxed) {
            log::info!("drop_caches: {}", value);
        }

        Ok(data.len())
    }
}

impl FileOps for DropCachesFileOps {
    fn read_at(
        &self,
        _offset: usize,
        _len: usize,
        _buf: &mut [u8],
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        Err(SystemError::EACCES)
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

/// /proc/sys/vm/oom_fault_inject 文件的 FileOps 实现
///
/// DragonOS-only test hook. It is intentionally restricted to privileged
/// callers and must not be treated as a Linux-compatible sysctl ABI.
#[derive(Debug)]
pub struct OomFaultInjectFileOps;

impl OomFaultInjectFileOps {
    pub fn new_inode(parent: Weak<dyn IndexNode>) -> Arc<dyn IndexNode> {
        ProcFileBuilder::new(Self, InodeMode::from_bits_truncate(0o600))
            .parent(parent)
            .build()
            .unwrap()
    }
}

impl FileOps for OomFaultInjectFileOps {
    fn read_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        if !capable(CAPFlags::CAP_SYS_ADMIN) {
            return Err(SystemError::EPERM);
        }
        let content = crate::mm::oom::read_fault_inject_config();
        crate::filesystem::procfs::utils::proc_read(offset, len, buf, content.as_bytes())
    }

    fn write_at(
        &self,
        _offset: usize,
        _len: usize,
        buf: &[u8],
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        if !capable(CAPFlags::CAP_SYS_ADMIN) {
            return Err(SystemError::EPERM);
        }
        crate::mm::oom::write_fault_inject_config(buf)
    }
}
