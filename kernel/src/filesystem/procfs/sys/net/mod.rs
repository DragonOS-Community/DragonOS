mod ipv4;

use crate::filesystem::{
    procfs::template::{DirOps, ProcDir, ProcDirBuilder},
    vfs::{IndexNode, InodeMode},
};
use alloc::string::ToString;
use alloc::sync::{Arc, Weak};
use ipv4::Ipv4DirOps;
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
    fn lookup_child(
        &self,
        dir: &ProcDir<Self>,
        name: &str,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        if name == "ipv4" {
            let mut cached_children = dir.cached_children().write();
            if let Some(child) = cached_children.get(name) {
                return Ok(child.clone());
            }

            let inode = Ipv4DirOps::new_inode(dir.self_ref_weak().clone());
            cached_children.insert(name.to_string(), inode.clone());
            return Ok(inode);
        }

        Err(SystemError::ENOENT)
    }

    fn populate_children(&self, dir: &ProcDir<Self>) {
        let mut cached_children = dir.cached_children().write();
        cached_children
            .entry("ipv4".to_string())
            .or_insert_with(|| Ipv4DirOps::new_inode(dir.self_ref_weak().clone()));
    }
}
