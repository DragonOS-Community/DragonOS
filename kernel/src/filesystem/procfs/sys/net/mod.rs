mod bridge;
mod ipv4;
mod ipv6;
mod namespace;

use crate::filesystem::{
    procfs::template::{DirOps, ProcDir, ProcDirBuilder},
    vfs::{IndexNode, InodeMode},
};
use alloc::string::ToString;
use alloc::sync::{Arc, Weak};
use bridge::BridgeDirOps;
use ipv4::Ipv4DirOps;
use ipv6::Ipv6DirOps;
use system_error::SystemError;

use crate::filesystem::procfs::Builder;

#[derive(Debug)]
pub struct NetDirOps;

impl NetDirOps {
    pub fn new_inode(parent: Weak<dyn IndexNode>) -> Arc<dyn IndexNode> {
        ProcDirBuilder::new(Self, InodeMode::from_bits_truncate(0o555))
            .parent(parent)
            .build()
            .unwrap()
    }
}

impl DirOps for NetDirOps {
    fn validate_child(&self, child: &dyn IndexNode) -> bool {
        child
            .as_any_ref()
            .downcast_ref::<ProcDir<Ipv4DirOps>>()
            .is_none_or(|inode| inode.ops().is_current())
            && child
                .as_any_ref()
                .downcast_ref::<ProcDir<Ipv6DirOps>>()
                .is_none_or(|inode| inode.ops().is_current())
            && child
                .as_any_ref()
                .downcast_ref::<ProcDir<BridgeDirOps>>()
                .is_none_or(|inode| inode.ops().is_current())
    }
    fn lookup_child(
        &self,
        dir: &ProcDir<Self>,
        name: &str,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        let mut cached = dir.cached_children().write();
        if let Some(child) = cached.get(name) {
            if self.validate_child(child.as_ref()) {
                return Ok(child.clone());
            }
        }
        let parent = dir.self_ref_weak().clone();
        let inode = match name {
            "ipv4" => Ipv4DirOps::new_inode(parent),
            "ipv6" => Ipv6DirOps::new_inode(parent),
            "bridge" => BridgeDirOps::new_inode(parent),
            _ => return Err(SystemError::ENOENT),
        };
        cached.insert(name.to_string(), inode.clone());
        Ok(inode)
    }

    fn populate_children(&self, dir: &ProcDir<Self>) {
        for name in ["ipv4", "ipv6", "bridge"] {
            let _ = self.lookup_child(dir, name);
        }
    }
}
