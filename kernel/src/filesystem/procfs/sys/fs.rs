//! /proc/sys/fs - filesystem-wide kernel parameters.

use crate::{
    filesystem::{
        procfs::{
            template::{Builder, DirOps, FileOps, ProcDir, ProcDirBuilder, ProcFileBuilder},
            utils::proc_read,
        },
        vfs::{FilePrivateData, IndexNode, InodeMode},
    },
    libs::mutex::MutexGuard,
    process::{
        cred::SUID_DUMPABLE,
        namespace::mnt::{mount_max, set_mount_max},
        ProcessManager,
    },
};
use alloc::{
    format,
    string::ToString,
    sync::{Arc, Weak},
};
use core::sync::atomic::Ordering;
use system_error::SystemError;

use super::numeric::parse_numeric_sysctl;

#[derive(Debug)]
pub struct FsDirOps;

impl FsDirOps {
    pub fn new_inode(parent: Weak<dyn IndexNode>) -> Arc<dyn IndexNode> {
        ProcDirBuilder::new(Self, InodeMode::from_bits_truncate(0o555))
            .parent(parent)
            .build()
            .unwrap()
    }
}

impl DirOps for FsDirOps {
    fn lookup_child(
        &self,
        dir: &ProcDir<Self>,
        name: &str,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        if name != "mount-max" && name != "nr_open" && name != "suid_dumpable" {
            return Err(SystemError::ENOENT);
        }
        let mut cached_children = dir.cached_children().write();
        if let Some(child) = cached_children.get(name) {
            return Ok(child.clone());
        }
        let inode = match name {
            "mount-max" => MountMaxFileOps::new_inode(dir.self_ref_weak().clone()),
            "nr_open" => NrOpenFileOps::new_inode(dir.self_ref_weak().clone()),
            _ => SuidDumpableFileOps::new_inode(dir.self_ref_weak().clone()),
        };
        cached_children.insert(name.to_string(), inode.clone());
        Ok(inode)
    }

    fn populate_children(&self, dir: &ProcDir<Self>) {
        let mut cached_children = dir.cached_children().write();
        let self_weak = dir.self_ref_weak().clone();
        cached_children
            .entry("mount-max".to_string())
            .or_insert_with(|| MountMaxFileOps::new_inode(self_weak.clone()));
        cached_children
            .entry("nr_open".to_string())
            .or_insert_with(|| NrOpenFileOps::new_inode(self_weak.clone()));
        cached_children
            .entry("suid_dumpable".to_string())
            .or_insert_with(|| SuidDumpableFileOps::new_inode(self_weak));
    }
}

#[derive(Debug)]
struct MountMaxFileOps;

impl MountMaxFileOps {
    fn new_inode(parent: Weak<dyn IndexNode>) -> Arc<dyn IndexNode> {
        ProcFileBuilder::new(Self, InodeMode::from_bits_truncate(0o644))
            .parent(parent)
            .build()
            .unwrap()
    }
}

impl FileOps for MountMaxFileOps {
    fn read_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        // Numeric proc sysctls return EOF after the first read, regardless of
        // where that non-zero offset falls in their textual representation.
        if offset != 0 {
            return Ok(0);
        }
        let content = format!("{}\n", mount_max());
        proc_read(offset, len, buf, content.as_bytes())
    }

    fn write_at(
        &self,
        offset: usize,
        _len: usize,
        buf: &[u8],
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        // Linux rechecks a 0644 sysctl against the caller's global effective
        // UID on every write. In particular, capabilities gained by entering
        // a child user namespace must not authorize this global parameter.
        if ProcessManager::current_pcb().cred().euid.data() != 0 {
            return Err(SystemError::EPERM);
        }
        // Linux strict numeric sysctls consume non-zero-offset writes without
        // changing the value.
        if offset != 0 {
            return Ok(buf.len());
        }
        let (value, consumed) = parse_numeric_sysctl(buf)?;
        if !(1..=i32::MAX as i64).contains(&value) {
            return Err(SystemError::EINVAL);
        }
        set_mount_max(value as u32)?;
        Ok(consumed)
    }
}

#[derive(Debug)]
struct NrOpenFileOps;

impl NrOpenFileOps {
    fn new_inode(parent: Weak<dyn IndexNode>) -> Arc<dyn IndexNode> {
        ProcFileBuilder::new(Self, InodeMode::from_bits_truncate(0o644))
            .parent(parent)
            .build()
            .unwrap()
    }
}

impl FileOps for NrOpenFileOps {
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
        let content = format!("{}\n", crate::filesystem::vfs::fdtable::nr_open());
        proc_read(offset, len, buf, content.as_bytes())
    }

    fn write_at(
        &self,
        offset: usize,
        _len: usize,
        buf: &[u8],
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        if ProcessManager::current_pcb().cred().euid.data() != 0 {
            return Err(SystemError::EPERM);
        }
        if offset != 0 {
            return Ok(buf.len());
        }
        let (value, consumed) = parse_numeric_sysctl(buf)?;
        let value = usize::try_from(value).map_err(|_| SystemError::EINVAL)?;
        crate::filesystem::vfs::fdtable::set_nr_open(value)?;
        Ok(consumed)
    }
}

#[derive(Debug)]
struct SuidDumpableFileOps;

impl SuidDumpableFileOps {
    fn new_inode(parent: Weak<dyn IndexNode>) -> Arc<dyn IndexNode> {
        ProcFileBuilder::new(Self, InodeMode::from_bits_truncate(0o644))
            .parent(parent)
            .build()
            .unwrap()
    }
}

impl FileOps for SuidDumpableFileOps {
    fn read_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        // Numeric proc sysctls return EOF after the first read, regardless of
        // where that non-zero offset falls in their textual representation.
        if offset != 0 {
            return Ok(0);
        }
        let content = format!("{}\n", SUID_DUMPABLE.load(Ordering::SeqCst));
        proc_read(offset, len, buf, content.as_bytes())
    }

    fn write_at(
        &self,
        offset: usize,
        _len: usize,
        buf: &[u8],
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        let cred = ProcessManager::current_pcb().cred();
        if cred.fsuid.data() != 0
            && !crate::process::cred::capable(crate::process::cred::CAPFlags::CAP_DAC_OVERRIDE)
        {
            return Err(SystemError::EPERM);
        }
        if offset != 0 {
            return Ok(buf.len());
        }
        let (value, consumed) = parse_numeric_sysctl(buf)?;
        // Only 0/1/2 are valid values
        if !(0..=2).contains(&value) {
            return Err(SystemError::EINVAL);
        }
        SUID_DUMPABLE.store(value as i32, Ordering::SeqCst);
        Ok(consumed)
    }
}
