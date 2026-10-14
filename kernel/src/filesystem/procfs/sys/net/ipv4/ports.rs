//! The two related INET port sysctls share ownership, permissions and parsing.

use alloc::{
    format,
    sync::{Arc, Weak},
};
use system_error::SystemError;

use super::super::super::numeric::parse_numeric_sysctl_values;
use crate::{
    filesystem::{
        procfs::{
            template::{Builder, FileOps, ProcFileBuilder},
            utils::proc_read,
        },
        vfs::{file::FileFlags, FilePrivateData, IndexNode, InodeMode},
    },
    libs::mutex::MutexGuard,
    process::{
        cred::{ns_capable, CAPFlags},
        namespace::{
            net_namespace::NetNamespace,
            user_namespace::{make_kgid, make_kuid},
        },
        ProcessManager,
    },
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
    // Cached inodes must not retain a namespace after its last real owner.
    netns: Weak<NetNamespace>,
}

impl PortFileOps {
    pub(super) fn new_inode(kind: PortSysctl, parent: Weak<dyn IndexNode>) -> Arc<dyn IndexNode> {
        ProcFileBuilder::new(
            Self {
                kind,
                netns: Arc::downgrade(&ProcessManager::current_netns()),
            },
            InodeMode::from_bits_truncate(0o644),
        )
        .parent(parent)
        .sysctl_permissions()
        .build()
        .unwrap()
    }

    pub(super) fn is_current(&self) -> bool {
        self.netns
            .upgrade()
            .is_some_and(|ns| Arc::ptr_eq(&ns, &ProcessManager::current_netns()))
    }

    fn access_bits(netns: &Arc<NetNamespace>) -> u32 {
        if ProcessManager::current_pcb().cred().euid.data() == 0
            || ns_capable(netns.user_ns(), CAPFlags::CAP_NET_ADMIN)
        {
            6
        } else {
            4
        }
    }

    fn fd_netns(data: &FilePrivateData) -> Result<&Arc<NetNamespace>, SystemError> {
        match data {
            FilePrivateData::Procfs(data) => data.net_ns.as_ref().ok_or(SystemError::ENOENT),
            _ => Err(SystemError::EINVAL),
        }
    }
}

impl FileOps for PortFileOps {
    fn sysctl_access_bits(&self) -> Option<u32> {
        Some(self.netns.upgrade().map_or(0, |ns| Self::access_bits(&ns)))
    }

    fn owner(&self) -> Option<(usize, usize)> {
        let ns = self.netns.upgrade()?;
        let user_ns = ns.user_ns();
        let uid = make_kuid(user_ns, 0).map_or(0, |uid| uid.data());
        let gid = make_kgid(user_ns, 0).map_or(0, |gid| gid.data());
        Some((uid, gid))
    }

    fn open(
        &self,
        data: &mut MutexGuard<FilePrivateData>,
        _flags: &FileFlags,
    ) -> Result<(), SystemError> {
        let ns = self.netns.upgrade().ok_or(SystemError::ENOENT)?;
        let FilePrivateData::Procfs(data) = &mut **data else {
            return Err(SystemError::EINVAL);
        };
        data.net_ns = Some(ns);
        Ok(())
    }

    fn read_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        if offset != 0 {
            return Ok(0);
        }
        let ns = Self::fd_netns(&data)?;
        let text = match self.kind {
            PortSysctl::LocalRange => {
                let (min, max) = ns.local_port_range();
                format!("{min}\t{max}\n")
            }
            PortSysctl::UnprivilegedStart => format!("{}\n", ns.unprivileged_port_start()),
        };
        proc_read(0, len, buf, text.as_bytes())
    }

    fn write_at(
        &self,
        offset: usize,
        _len: usize,
        buf: &[u8],
        data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        let ns = Self::fd_netns(&data)?;
        if Self::access_bits(ns) & 2 == 0 {
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
