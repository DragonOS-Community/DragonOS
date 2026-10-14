use crate::filesystem::{
    procfs::{
        template::{Builder, DirOps, FileOps, ProcDir, ProcDirBuilder, ProcFile, ProcFileBuilder},
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
mod ports;
use ports::{PortFileOps, PortSysctl};

#[derive(Debug)]
pub struct Ipv4DirOps;

impl Ipv4DirOps {
    pub fn new_inode(parent: Weak<dyn IndexNode>) -> Arc<dyn IndexNode> {
        ProcDirBuilder::new(Self, InodeMode::from_bits_truncate(0o555))
            .parent(parent)
            .build()
            .unwrap()
    }
}

impl DirOps for Ipv4DirOps {
    fn validate_child(&self, child: &dyn IndexNode) -> bool {
        child
            .as_any_ref()
            .downcast_ref::<ProcFile<PortFileOps>>()
            .is_none_or(|inode| inode.ops().is_current())
    }
    fn lookup_child(
        &self,
        dir: &ProcDir<Self>,
        name: &str,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        if PortSysctl::from_name(name).is_some() || name == "ip_forward" {
            let mut cached_children = dir.cached_children().write();
            if let Some(child) = cached_children.get(name) {
                if self.validate_child(child.as_ref()) {
                    return Ok(child.clone());
                }
            }

            let inode = if name == "ip_forward" {
                IpForwardFileOps::new_inode(dir.self_ref_weak().clone())
            } else {
                PortFileOps::new_inode(
                    PortSysctl::from_name(name).unwrap(),
                    dir.self_ref_weak().clone(),
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
struct IpForwardFileOps;

impl IpForwardFileOps {
    fn new_inode(parent: Weak<dyn IndexNode>) -> Arc<dyn IndexNode> {
        ProcFileBuilder::new(Self, InodeMode::from_bits_truncate(0o644))
            .parent(parent)
            .sysctl_permissions()
            .build()
            .unwrap()
    }
}

impl FileOps for IpForwardFileOps {
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
        let value = ProcessManager::current_netns().ipv4_forwarding_value();
        proc_read(0, len, buf, format!("{value}\n").as_bytes())
    }

    fn write_at(
        &self,
        offset: usize,
        _len: usize,
        buf: &[u8],
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        let netns = ProcessManager::current_netns();
        if !ProcessManager::current_pcb()
            .cred()
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
        netns.set_ipv4_forwarding(value);
        Ok(consumed)
    }
}
