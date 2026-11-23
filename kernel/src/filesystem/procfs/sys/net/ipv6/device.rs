//! A per-device sysctl inode tracks one registration under one name.

use alloc::{
    format,
    string::ToString,
    sync::{Arc, Weak},
};
use system_error::SystemError;

use crate::{
    driver::net::Iface,
    filesystem::{
        procfs::{
            template::{
                Builder, DirOps, FileOps, ProcDir, ProcDirBuilder, ProcFileBuilder,
                ProcWriteCallback,
            },
            utils::proc_read,
        },
        vfs::{file::FileFlags, FilePrivateData, IndexNode, InodeMode, PrimaryWriteResult},
    },
    libs::mutex::MutexGuard,
    net::{address::set_device_ipv6_disabled, rtnl},
    process::namespace::net_namespace::NetNamespace,
};

use super::{parse_numeric_sysctl, SysctlNamespace};

#[derive(Debug, Clone)]
pub(super) struct DeviceIdentity {
    namespace: SysctlNamespace,
    iface: Weak<dyn Iface>,
    namespace_epoch: u64,
    name_generation: u64,
}

impl DeviceIdentity {
    pub(super) fn new(namespace: SysctlNamespace, iface: &Arc<dyn Iface>) -> Self {
        Self {
            namespace,
            iface: Arc::downgrade(iface),
            namespace_epoch: iface.common().namespace_epoch(),
            name_generation: iface.common().name_generation(),
        }
    }

    fn resolve(&self) -> Result<(Arc<NetNamespace>, Arc<dyn Iface>), SystemError> {
        let netns = self.namespace.upgrade()?;
        let iface = self.iface.upgrade().ok_or(SystemError::ENOENT)?;
        if self.namespace_epoch & 1 != 0
            || iface.common().namespace_epoch() != self.namespace_epoch
            || iface.common().name_generation() != self.name_generation
            || !iface
                .net_namespace()
                .is_some_and(|owner| Arc::ptr_eq(&owner, &netns))
        {
            return Err(SystemError::ENOENT);
        }
        Ok((netns, iface))
    }
}

#[derive(Debug)]
pub(super) struct DeviceDirOps(DeviceIdentity);

impl DeviceDirOps {
    pub(super) fn new_inode(
        parent: Weak<dyn IndexNode>,
        identity: DeviceIdentity,
    ) -> Arc<dyn IndexNode> {
        ProcDirBuilder::new(Self(identity), InodeMode::from_bits_truncate(0o555))
            .parent(parent)
            .build()
            .unwrap()
    }

    pub(super) fn has_identity(&self, identity: &DeviceIdentity) -> bool {
        Weak::ptr_eq(&self.0.iface, &identity.iface)
            && self.0.namespace_epoch == identity.namespace_epoch
            && self.0.name_generation == identity.name_generation
    }
}

impl DirOps for DeviceDirOps {
    fn validate_inode(&self) -> Result<(), SystemError> {
        self.0.resolve().map(|_| ())
    }

    fn before_read_dir(&self) -> Result<(), SystemError> {
        self.validate_inode()
    }

    fn owner(&self) -> Option<(usize, usize)> {
        self.0.namespace.owner()
    }

    fn lookup_child(
        &self,
        dir: &ProcDir<Self>,
        name: &str,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        self.validate_inode()?;
        let field = match name {
            "disable_ipv6" => DeviceField::Disable,
            "forwarding" => DeviceField::Forwarding,
            _ => return Err(SystemError::ENOENT),
        };
        Ok(dir
            .cached_children()
            .write()
            .entry(name.to_string())
            .or_insert_with(|| {
                ProcFileBuilder::new(
                    DeviceFileOps {
                        identity: self.0.clone(),
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
        for name in ["disable_ipv6", "forwarding"] {
            let _ = self.lookup_child(dir, name);
        }
    }
}

#[derive(Debug)]
enum DeviceField {
    Disable,
    Forwarding,
}

#[derive(Debug)]
struct DeviceFileOps {
    identity: DeviceIdentity,
    field: DeviceField,
}

impl FileOps for DeviceFileOps {
    const SEEK_KIND: crate::filesystem::procfs::template::ProcfsSeekKind =
        crate::filesystem::procfs::template::ProcfsSeekKind::Default;

    fn validate_inode(&self) -> Result<(), SystemError> {
        self.identity.resolve().map(|_| ())
    }

    fn open(
        &self,
        _data: &mut MutexGuard<FilePrivateData>,
        _flags: &FileFlags,
    ) -> Result<(), SystemError> {
        self.validate_inode()
    }

    fn sysctl_access_bits(&self) -> Result<Option<u32>, SystemError> {
        let (netns, _) = self.identity.resolve()?;
        Ok(Some(SysctlNamespace::access_bits_for(&netns)))
    }

    fn owner(&self) -> Option<(usize, usize)> {
        self.identity.namespace.owner()
    }

    fn supports_write_iter_flags(&self) -> bool {
        true
    }

    fn write_callback(&self) -> Option<ProcWriteCallback<Self>> {
        Some(|ops, offset, _, buf, _data| {
            let rtnl = rtnl::lock();
            let (netns, iface) = ops.identity.resolve()?;
            if SysctlNamespace::access_bits_for(&netns) & 2 == 0 {
                return Err(SystemError::EPERM);
            }
            if buf.is_empty() || offset != 0 {
                return Ok(PrimaryWriteResult::new(buf.len()));
            }
            let (value, consumed) = parse_numeric_sysctl(buf)?;
            let value = i32::try_from(value).map_err(|_| SystemError::EINVAL)?;
            match ops.field {
                DeviceField::Disable => set_device_ipv6_disabled(&rtnl, &netns, &iface, value)?,
                DeviceField::Forwarding => iface.common().set_ipv6_forwarding(&rtnl, value),
            }
            Ok(PrimaryWriteResult::new(consumed))
        })
    }

    fn read_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        let _rtnl = rtnl::lock();
        let (_, iface) = self.identity.resolve()?;
        if offset != 0 {
            return Ok(0);
        }
        let value = match self.field {
            DeviceField::Disable => iface.common().ipv6_disabled_value(),
            DeviceField::Forwarding => iface.common().ipv6_forwarding(),
        };
        proc_read(0, len, buf, format!("{value}\n").as_bytes())
    }
}
