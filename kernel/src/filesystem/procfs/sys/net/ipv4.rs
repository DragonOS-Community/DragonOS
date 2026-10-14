use crate::filesystem::{
    procfs::{
        template::{Builder, DirOps, FileOps, ProcDir, ProcDirBuilder, ProcFileBuilder},
        utils::proc_read,
    },
    vfs::{file::FileFlags, FilePrivateData, IndexNode, InodeMode},
};
use crate::libs::mutex::MutexGuard;
use crate::process::cred::{ns_capable, CAPFlags};
use alloc::{
    format,
    string::ToString,
    sync::{Arc, Weak},
};
use system_error::SystemError;

use super::super::numeric::parse_numeric_sysctl;
mod namespace;
mod ports;
use namespace::SysctlNamespace;
use ports::{PortFileOps, PortSysctl};

#[derive(Debug)]
pub struct Ipv4DirOps {
    namespace: SysctlNamespace,
}

impl Ipv4DirOps {
    pub fn new_inode(parent: Weak<dyn IndexNode>) -> Arc<dyn IndexNode> {
        ProcDirBuilder::new(
            Self {
                namespace: SysctlNamespace::current(),
            },
            InodeMode::from_bits_truncate(0o555),
        )
        .parent(parent)
        .build()
        .unwrap()
    }

    pub(super) fn is_current(&self) -> bool {
        self.namespace.is_current()
    }
}

impl DirOps for Ipv4DirOps {
    fn before_read_dir(&self) -> Result<(), SystemError> {
        self.namespace.upgrade().map(|_| ())
    }
    fn validate_child(&self, _child: &dyn IndexNode) -> bool {
        // Expired namespace tables must go through lookup_child's ENOENT path.
        self.namespace.upgrade().is_ok()
    }
    fn lookup_child(
        &self,
        dir: &ProcDir<Self>,
        name: &str,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        if PortSysctl::from_name(name).is_some() || name == "ip_forward" {
            // A retained directory addresses its original table, never the
            // calling task's new namespace. Check liveness before cached leaves.
            let _target = self.namespace.upgrade()?;
            let mut cached_children = dir.cached_children().write();
            if let Some(child) = cached_children.get(name) {
                return Ok(child.clone());
            }

            let inode = if name == "ip_forward" {
                IpForwardFileOps::new_inode(dir.self_ref_weak().clone(), self.namespace.clone())
            } else {
                PortFileOps::new_inode(
                    PortSysctl::from_name(name).unwrap(),
                    dir.self_ref_weak().clone(),
                    self.namespace.clone(),
                )
            };
            cached_children.insert(name.to_string(), inode.clone());
            return Ok(inode);
        }

        Err(SystemError::ENOENT)
    }

    fn populate_children(&self, dir: &ProcDir<Self>) {
        for name in [
            "ip_local_port_range",
            "ip_unprivileged_port_start",
            "ip_forward",
        ] {
            let _ = self.lookup_child(dir, name);
        }
    }
}

#[derive(Debug)]
struct IpForwardFileOps {
    namespace: SysctlNamespace,
}

impl IpForwardFileOps {
    fn new_inode(parent: Weak<dyn IndexNode>, namespace: SysctlNamespace) -> Arc<dyn IndexNode> {
        ProcFileBuilder::new(Self { namespace }, InodeMode::from_bits_truncate(0o644))
            .parent(parent)
            .sysctl_permissions()
            .build()
            .unwrap()
    }
}

impl FileOps for IpForwardFileOps {
    fn sysctl_access_bits(&self) -> Result<Option<u32>, SystemError> {
        self.namespace.access_bits().map(Some)
    }

    fn owner(&self) -> Option<(usize, usize)> {
        self.namespace.owner()
    }

    fn open(
        &self,
        data: &mut MutexGuard<FilePrivateData>,
        _flags: &FileFlags,
    ) -> Result<(), SystemError> {
        self.namespace.open(data)
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
        let value = SysctlNamespace::opened(&data)?.ipv4_forwarding_value();
        proc_read(0, len, buf, format!("{value}\n").as_bytes())
    }

    fn write_at(
        &self,
        offset: usize,
        _len: usize,
        buf: &[u8],
        data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        let netns = SysctlNamespace::opened(&data)?;
        // devinet_sysctl_forward has an additional handler capability check,
        // even when global-root mode bits allowed opening the file.
        if !ns_capable(netns.user_ns(), CAPFlags::CAP_NET_ADMIN) {
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
        netns.set_ipv4_forwarding(value);
        Ok(consumed)
    }
}
