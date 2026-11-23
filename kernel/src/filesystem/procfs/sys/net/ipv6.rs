//! Namespace and per-interface IPv6 sysctls.

mod device;

use crate::filesystem::procfs::template::ProcWriteCallback;
use crate::filesystem::procfs::Builder;
use crate::filesystem::{
    procfs::{
        template::{DirOps, FileOps, ProcDir, ProcDirBuilder, ProcFileBuilder},
        utils::proc_read,
    },
    vfs::{file::FileFlags, FilePrivateData, IndexNode, InodeMode, PrimaryWriteResult},
};
use crate::libs::mutex::MutexGuard;
use crate::net::rtnl;
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

#[derive(Debug, Clone, Copy)]
enum NamespaceForwarding {
    All,
    Default,
}

impl NamespaceForwarding {
    fn from_dir_name(name: &str) -> Option<Self> {
        match name {
            "all" => Some(Self::All),
            "default" => Some(Self::Default),
            _ => None,
        }
    }
}

#[derive(Debug)]
struct NamespaceConfDirOps {
    namespace: SysctlNamespace,
    target: NamespaceForwarding,
}

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

impl NamespaceConfDirOps {
    fn new_inode(
        parent: Weak<dyn IndexNode>,
        namespace: SysctlNamespace,
        target: NamespaceForwarding,
    ) -> Arc<dyn IndexNode> {
        ProcDirBuilder::new(
            Self { namespace, target },
            InodeMode::from_bits_truncate(0o555),
        )
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
        if let Some(target) = NamespaceForwarding::from_dir_name(name) {
            return Ok(dir
                .cached_children()
                .write()
                .entry(name.to_string())
                .or_insert_with(|| {
                    NamespaceConfDirOps::new_inode(
                        dir.self_ref_weak().clone(),
                        self.0.clone(),
                        target,
                    )
                })
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
        for name in ["all", "default"] {
            let _ = self.lookup_child(dir, name);
        }
        for iface in netns.device_list().values() {
            iface.common().with_iface_name(|name| {
                if NamespaceForwarding::from_dir_name(name).is_none() {
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

impl DirOps for NamespaceConfDirOps {
    fn validate_inode(&self) -> Result<(), SystemError> {
        self.namespace.upgrade().map(|_| ())
    }

    fn before_read_dir(&self) -> Result<(), SystemError> {
        self.validate_inode()
    }

    fn lookup_child(
        &self,
        dir: &ProcDir<Self>,
        name: &str,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        self.validate_inode()?;
        if name != "forwarding" {
            return Err(SystemError::ENOENT);
        }
        Ok(dir
            .cached_children()
            .write()
            .entry(name.to_string())
            .or_insert_with(|| {
                ProcFileBuilder::new(
                    ForwardingFileOps {
                        namespace: self.namespace.clone(),
                        target: self.target,
                    },
                    InodeMode::from_bits_truncate(0o644),
                )
                .parent(dir.self_ref_weak().clone())
                .sysctl_permissions()
                .build()
                .unwrap()
            })
            .clone())
    }

    fn populate_children(&self, dir: &ProcDir<Self>) {
        let _ = self.lookup_child(dir, "forwarding");
    }
}

#[derive(Debug)]
struct ForwardingFileOps {
    namespace: SysctlNamespace,
    target: NamespaceForwarding,
}

impl FileOps for ForwardingFileOps {
    const SEEK_KIND: crate::filesystem::procfs::template::ProcfsSeekKind =
        crate::filesystem::procfs::template::ProcfsSeekKind::Default;

    fn validate_inode(&self) -> Result<(), SystemError> {
        self.namespace.upgrade().map(|_| ())
    }

    fn open(
        &self,
        _data: &mut MutexGuard<FilePrivateData>,
        _flags: &FileFlags,
    ) -> Result<(), SystemError> {
        self.validate_inode()
    }

    fn sysctl_access_bits(&self) -> Result<Option<u32>, SystemError> {
        self.namespace.access_bits().map(Some)
    }

    fn owner(&self) -> Option<(usize, usize)> {
        self.namespace.owner()
    }

    fn write_callback(&self) -> Option<ProcWriteCallback<Self>> {
        Some(|ops, offset, _, buf, _data| {
            let netns = ops.namespace.upgrade()?;
            if SysctlNamespace::access_bits_for(&netns) & 2 == 0 {
                return Err(SystemError::EPERM);
            }
            if buf.is_empty() || offset != 0 {
                return Ok(PrimaryWriteResult::new(buf.len()));
            }
            let (value, consumed) = parse_numeric_sysctl(buf)?;
            let value = i32::try_from(value).map_err(|_| SystemError::EINVAL)?;
            let rtnl = rtnl::lock();
            match ops.target {
                NamespaceForwarding::All => netns.set_ipv6_forwarding(&rtnl, value),
                NamespaceForwarding::Default => netns.set_ipv6_default_forwarding(&rtnl, value),
            }
            Ok(PrimaryWriteResult::new(consumed))
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
        let netns = self.namespace.upgrade()?;
        if offset != 0 {
            return Ok(0);
        }
        let value = match self.target {
            NamespaceForwarding::All => netns.ipv6_forwarding_value(),
            NamespaceForwarding::Default => netns.ipv6_default_forwarding(),
        };
        proc_read(0, len, buf, format!("{value}\n").as_bytes())
    }
}
