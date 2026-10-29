use core::fmt::Debug;

use crate::filesystem::{
    procfs::template::{Builder, ProcSymBuilder, SymOps},
    vfs::{IndexNode, InodeMode},
};
use alloc::sync::{Arc, Weak};
use system_error::SystemError;

/// `/proc/mounts` -> `self/mounts`
#[derive(Debug)]
pub struct MountsSymOps;

impl MountsSymOps {
    pub fn new_inode(parent: Weak<dyn IndexNode>) -> Arc<dyn IndexNode> {
        ProcSymBuilder::new(Self, InodeMode::S_IRUGO)
            .parent(parent)
            .build()
            .unwrap()
    }
}

impl SymOps for MountsSymOps {
    fn read_link(&self, buf: &mut [u8]) -> Result<usize, SystemError> {
        const TARGET: &[u8] = b"self/mounts";
        let len = TARGET.len().min(buf.len());
        buf[..len].copy_from_slice(&TARGET[..len]);
        Ok(len)
    }
}
