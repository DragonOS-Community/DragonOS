//! The two related INET port sysctls share ownership, permissions and parsing.

use crate::filesystem::procfs::template::ProcWriteCallback;
use alloc::{
    format,
    sync::{Arc, Weak},
};
use system_error::SystemError;

use super::super::super::numeric::parse_numeric_sysctl_values;
use super::namespace::SysctlNamespace;
use crate::{
    filesystem::{
        procfs::{
            template::{Builder, FileOps, ProcFileBuilder},
            utils::proc_read,
        },
        vfs::{file::FileFlags, FilePrivateData, IndexNode, InodeMode},
    },
    libs::mutex::MutexGuard,
};

#[derive(Debug, Clone, Copy)]
pub(super) enum PortSysctl {
    LocalRange,
    UnprivilegedStart,
}

impl PortSysctl {
    pub(super) fn from_name(name: &str) -> Option<Self> {
        match name {
            "ip_local_port_range" => Some(Self::LocalRange),
            "ip_unprivileged_port_start" => Some(Self::UnprivilegedStart),
            _ => None,
        }
    }
}

#[derive(Debug)]
pub(super) struct PortFileOps {
    kind: PortSysctl,
    namespace: SysctlNamespace,
}

impl PortFileOps {
    pub(super) fn new_inode(
        kind: PortSysctl,
        parent: Weak<dyn IndexNode>,
        namespace: SysctlNamespace,
    ) -> Arc<dyn IndexNode> {
        ProcFileBuilder::new(
            Self { kind, namespace },
            InodeMode::from_bits_truncate(0o644),
        )
        .parent(parent)
        .sysctl_permissions()
        .build()
        .unwrap()
    }
}

impl FileOps for PortFileOps {
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

    fn validate_inode(&self) -> Result<(), SystemError> {
        self.namespace.upgrade().map(|_| ())
    }
    fn sysctl_access_bits(&self) -> Result<Option<u32>, SystemError> {
        self.namespace.access_bits().map(Some)
    }

    fn owner(&self) -> Option<(usize, usize)> {
        self.namespace.owner()
    }

    fn open(
        &self,
        _data: &mut MutexGuard<FilePrivateData>,
        _flags: &FileFlags,
    ) -> Result<(), SystemError> {
        self.validate_inode()
    }

    fn read_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        let ns = self.namespace.upgrade()?;
        if offset != 0 {
            return Ok(0);
        }
        let text = match self.kind {
            PortSysctl::LocalRange => {
                let (min, max) = ns.local_port_range();
                format!("{min}\t{max}\n")
            }
            PortSysctl::UnprivilegedStart => format!("{}\n", ns.unprivileged_port_start()),
        };
        proc_read(0, len, buf, text.as_bytes())
    }
}

impl PortFileOps {
    fn write_at(
        &self,
        offset: usize,
        _len: usize,
        buf: &[u8],
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        let ns = self.namespace.upgrade()?;
        if SysctlNamespace::access_bits_for(&ns) & 2 == 0 {
            return Err(SystemError::EPERM);
        }
        if buf.is_empty() {
            return Ok(0);
        }
        if offset != 0 {
            return Ok(buf.len());
        }
        let (values, consumed) = match self.kind {
            PortSysctl::LocalRange => parse_numeric_sysctl_values::<2>(buf)?,
            PortSysctl::UnprivilegedStart => {
                let (values, consumed) = parse_numeric_sysctl_values::<1>(buf)?;
                ([values[0], None], consumed)
            }
        };
        let first = u16::try_from(values[0].ok_or(SystemError::EINVAL)?)
            .map_err(|_| SystemError::EINVAL)?;
        match self.kind {
            PortSysctl::LocalRange => {
                let max = values[1]
                    .map(u16::try_from)
                    .transpose()
                    .map_err(|_| SystemError::EINVAL)?
                    .unwrap_or_else(|| ns.local_port_range().1);
                ns.set_local_port_range(first, max)?;
            }
            PortSysctl::UnprivilegedStart => ns.set_unprivileged_port_start(first)?,
        }
        Ok(consumed)
    }
}
