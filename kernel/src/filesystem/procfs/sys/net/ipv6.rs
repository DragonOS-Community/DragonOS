//! Namespace and per-interface IPv6 sysctls.

mod device;

use crate::filesystem::procfs::template::ProcWriteCallback;
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
use super::namespace::SysctlNamespace;
use device::DeviceDirOps;

#[derive(Debug)]
pub(super) struct Ipv6DirOps(SysctlNamespace);

#[derive(Debug)]
struct ConfDirOps(SysctlNamespace);

#[derive(Debug)]
struct AllDirOps;

impl Ipv6DirOps {
    pub(super) fn new_inode(parent: Weak<dyn IndexNode>) -> Arc<dyn IndexNode> {
        ProcDirBuilder::new(
            Self(SysctlNamespace::current()),
            InodeMode::from_bits_truncate(0o555),
        )
        .parent(parent)
        .build()
        .unwrap()
    }

    pub(super) fn is_current(&self) -> bool {
        self.0.is_current()
    }
}

impl ConfDirOps {
    fn new_inode(parent: Weak<dyn IndexNode>, namespace: SysctlNamespace) -> Arc<dyn IndexNode> {
        ProcDirBuilder::new(Self(namespace), InodeMode::from_bits_truncate(0o555))
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
            .or_insert_with(|| ConfDirOps::new_inode(dir.self_ref_weak().clone(), self.0.clone()))
            .clone())
    }

    fn populate_children(&self, dir: &ProcDir<Self>) {
        dir.cached_children()
            .write()
            .entry("conf".to_string())
            .or_insert_with(|| ConfDirOps::new_inode(dir.self_ref_weak().clone(), self.0.clone()));
    }
}

impl DirOps for ConfDirOps {
    fn validate_inode(&self) -> Result<(), SystemError> {
        self.0.upgrade().map(|_| ())
    }

    fn before_read_dir(&self) -> Result<(), SystemError> {
        self.validate_inode()
    }

    fn validate_child(&self, child: &dyn IndexNode) -> bool {
        child
            .as_any_ref()
            .downcast_ref::<ProcDir<DeviceDirOps>>()
            .is_none_or(|inode| inode.ops().validate_inode().is_ok())
    }

    fn lookup_child(
        &self,
        dir: &ProcDir<Self>,
        name: &str,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        let netns = self.0.upgrade()?;
        if name == "all" {
            return Ok(dir
                .cached_children()
                .write()
                .entry(name.to_string())
                .or_insert_with(|| AllDirOps::new_inode(dir.self_ref_weak().clone()))
                .clone());
        }
        let devices = netns.device_list();
        for iface in devices.values() {
            let child = iface.common().with_iface_name(|current_name| {
                (current_name == name).then(|| self.cache_device(dir, name, iface))
            });
            if let Some(child) = child {
                return Ok(child);
            }
        }
        Err(SystemError::ENOENT)
    }

    fn populate_children(&self, dir: &ProcDir<Self>) {
        let Ok(netns) = self.0.upgrade() else {
            return;
        };
        let _ = self.lookup_child(dir, "all");
        for iface in netns.device_list().values() {
            iface.common().with_iface_name(|name| {
                if name != "all" {
                    self.cache_device(dir, name, iface);
                }
            });
        }
        dir.cached_children()
            .write()
            .retain(|_, child| self.validate_child(child.as_ref()));
    }
}

impl ConfDirOps {
    fn cache_device(
        &self,
        dir: &ProcDir<Self>,
        name: &str,
        iface: &Arc<dyn crate::driver::net::Iface>,
    ) -> Arc<dyn IndexNode> {
        let mut children = dir.cached_children().write();
        let identity = device::DeviceIdentity::new(self.0.clone(), iface);
        if let Some(child) = children.get(name) {
            if child
                .as_any_ref()
                .downcast_ref::<ProcDir<DeviceDirOps>>()
                .is_some_and(|inode| inode.ops().has_identity(&identity))
            {
                return child.clone();
            }
        }
        let inode = DeviceDirOps::new_inode(dir.self_ref_weak().clone(), identity);
        children.insert(name.to_string(), inode.clone());
        inode
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
    const SEEK_KIND: crate::filesystem::procfs::template::ProcfsSeekKind =
        crate::filesystem::procfs::template::ProcfsSeekKind::Default;

    fn write_callback(&self) -> Option<ProcWriteCallback<Self>> {
        Some(|ops, offset, len, buf, data| {
            ops.write_at(offset, len, buf, data)
                .map(crate::filesystem::vfs::PrimaryWriteResult::new)
        })
    }

    fn supports_write_iter_flags(&self) -> bool {
        true
    }

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
}

impl ForwardingFileOps {
    fn write_at(
        &self,
        offset: usize,
        _len: usize,
        buf: &[u8],
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        let netns = ProcessManager::current_netns();
        if !ProcessManager::current_cred()
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
