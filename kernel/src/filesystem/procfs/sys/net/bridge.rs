//! Per-network-namespace controls for IP filtering on bridges.

use alloc::{
    string::ToString,
    sync::{Arc, Weak},
};
use smoltcp::wire::IpVersion;
use system_error::SystemError;

use crate::{
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
};

use super::{super::numeric::parse_numeric_sysctl, namespace::SysctlNamespace};

#[derive(Debug)]
pub(super) struct BridgeDirOps(SysctlNamespace);

impl BridgeDirOps {
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

impl DirOps for BridgeDirOps {
    fn validate_inode(&self) -> Result<(), SystemError> {
        self.0.upgrade().map(|_| ())
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
        let version = match name {
            "bridge-nf-call-iptables" => IpVersion::Ipv4,
            "bridge-nf-call-ip6tables" => IpVersion::Ipv6,
            _ => return Err(SystemError::ENOENT),
        };
        Ok(dir
            .cached_children()
            .write()
            .entry(name.to_string())
            .or_insert_with(|| {
                ProcFileBuilder::new(
                    BridgeFilterFile {
                        owner: self.0.clone(),
                        version,
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
        for name in ["bridge-nf-call-iptables", "bridge-nf-call-ip6tables"] {
            let _ = self.lookup_child(dir, name);
        }
    }
}

#[derive(Debug)]
struct BridgeFilterFile {
    owner: SysctlNamespace,
    version: IpVersion,
}

impl FileOps for BridgeFilterFile {
    const SEEK_KIND: crate::filesystem::procfs::template::ProcfsSeekKind =
        crate::filesystem::procfs::template::ProcfsSeekKind::Default;

    fn validate_inode(&self) -> Result<(), SystemError> {
        self.owner.upgrade().map(|_| ())
    }

    fn open(
        &self,
        _data: &mut MutexGuard<FilePrivateData>,
        _flags: &FileFlags,
    ) -> Result<(), SystemError> {
        self.validate_inode()
    }

    fn owner(&self) -> Option<(usize, usize)> {
        self.owner.owner()
    }

    fn sysctl_access_bits(&self) -> Result<Option<u32>, SystemError> {
        self.owner.access_bits().map(Some)
    }

    fn supports_write_iter_flags(&self) -> bool {
        true
    }

    fn write_callback(&self) -> Option<ProcWriteCallback<Self>> {
        Some(|ops, offset, _, buf, _data| {
            let netns = ops.owner.upgrade()?;
            if SysctlNamespace::access_bits_for(&netns) & 2 == 0 {
                return Err(SystemError::EPERM);
            }
            if buf.is_empty() || offset != 0 {
                return Ok(PrimaryWriteResult::new(buf.len()));
            }
            let (value, consumed) = parse_numeric_sysctl(buf)?;
            let value = i32::try_from(value).map_err(|_| SystemError::EINVAL)?;
            netns.set_bridge_netfilter(ops.version, value != 0);
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
        let netns = self.owner.upgrade()?;
        if offset != 0 {
            return Ok(0);
        }
        let bytes = if netns.bridge_netfilter_enabled(self.version) {
            b"1\n"
        } else {
            b"0\n"
        };
        proc_read(0, len, buf, bytes)
    }
}
