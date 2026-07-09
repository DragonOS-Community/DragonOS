use super::inode::OvlInode;
use crate::filesystem::vfs::IndexNode;
use alloc::sync::Arc;
use alloc::vec::Vec;

#[derive(Debug)]
#[allow(dead_code)]
pub(super) struct OvlEntry {
    lowerstack: Vec<OvlPath>,
}

impl OvlEntry {
    pub(super) fn new() -> Self {
        Self {
            lowerstack: Vec::new(),
        }
    }
}

#[derive(Debug)]
#[allow(dead_code)]
pub(super) struct OvlPath {
    layer: Arc<OvlLayer>,
    inode: Arc<dyn IndexNode>,
}

#[derive(Debug)]
#[allow(dead_code)]
pub(super) struct OvlLayer {
    pub(super) mnt: Arc<OvlInode>, // mount point
    pub(super) index: u32,         // 0 is the upper read-write layer, >0 are lower read-only layers
    pub(super) fsid: u32,          // filesystem identifier
}
