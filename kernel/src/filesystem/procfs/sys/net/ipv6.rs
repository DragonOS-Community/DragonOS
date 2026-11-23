//! Namespace and per-interface IPv6 sysctls.

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
use super::device_conf::{DeviceConfFamily, DeviceDirOps};
use super::namespace::SysctlNamespace;

#[derive(Debug)]
pub(super) struct Ipv6DirOps(SysctlNamespace);

#[derive(Debug)]
struct ConfDirOps(SysctlNamespace);

#[derive(Debug, Clone, Copy)]
enum NamespaceConfScope {
    All,
    Default,
}

impl NamespaceConfScope {
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
    target: NamespaceConfScope,
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
        target: NamespaceConfScope,
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
        DeviceDirOps::valid_child(child)
    }

    fn lookup_child(
        &self,
        dir: &ProcDir<Self>,
        name: &str,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        self.0.upgrade()?;
        if let Some(target) = NamespaceConfScope::from_dir_name(name) {
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
        DeviceDirOps::lookup(dir, &self.0, DeviceConfFamily::Ipv6, name)
    }

    fn populate_children(&self, dir: &ProcDir<Self>) {
        for name in ["all", "default"] {
            let _ = self.lookup_child(dir, name);
        }
        DeviceDirOps::populate(dir, &self.0, DeviceConfFamily::Ipv6);
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
        let field = match name {
            "forwarding" => NamespaceConfField::Forwarding,
            "accept_ra" => NamespaceConfField::AcceptRa,
            _ => return Err(SystemError::ENOENT),
        };
        Ok(dir
            .cached_children()
            .write()
            .entry(name.to_string())
            .or_insert_with(|| {
                ProcFileBuilder::new(
                    NamespaceConfFileOps {
                        namespace: self.namespace.clone(),
                        target: self.target,
                        field,
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
        let _ = self.lookup_child(dir, "accept_ra");
    }
}

#[derive(Debug)]
enum NamespaceConfField {
    Forwarding,
    AcceptRa,
}

#[derive(Debug)]
struct NamespaceConfFileOps {
    namespace: SysctlNamespace,
    target: NamespaceConfScope,
    field: NamespaceConfField,
}

impl FileOps for NamespaceConfFileOps {
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
            match (&ops.field, ops.target) {
                (NamespaceConfField::Forwarding, NamespaceConfScope::All) => {
                    crate::net::route::ra::set_forwarding(&rtnl, &netns, None, value)?;
                }
                (NamespaceConfField::Forwarding, NamespaceConfScope::Default) => {
                    netns.set_ipv6_default_forwarding(&rtnl, value);
                }
                (NamespaceConfField::AcceptRa, NamespaceConfScope::All) => {
                    netns.set_ipv6_all_accept_ra(&rtnl, value);
                }
                (NamespaceConfField::AcceptRa, NamespaceConfScope::Default) => {
                    netns.set_ipv6_default_accept_ra(&rtnl, value);
                }
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
        let value = match (&self.field, self.target) {
            (NamespaceConfField::Forwarding, NamespaceConfScope::All) => {
                netns.ipv6_forwarding_value()
            }
            (NamespaceConfField::Forwarding, NamespaceConfScope::Default) => {
                netns.ipv6_default_forwarding()
            }
            (NamespaceConfField::AcceptRa, NamespaceConfScope::All) => netns.ipv6_all_accept_ra(),
            (NamespaceConfField::AcceptRa, NamespaceConfScope::Default) => {
                netns.ipv6_default_accept_ra()
            }
        };
        proc_read(0, len, buf, format!("{value}\n").as_bytes())
    }
}
