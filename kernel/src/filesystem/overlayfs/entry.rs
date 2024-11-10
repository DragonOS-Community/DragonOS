use alloc::sync::Arc;

use alloc::vec::Vec;

use crate::filesystem::vfs::IndexNode;

use super::{OvlInode, OvlSuperBlock};
#[derive(Debug)]
pub struct OvlEntry {
    numlower: usize, // 下层数量
    lowerstack: Vec<OvlPath>,
}

impl OvlEntry {
    pub fn new() -> Self {
        Self {
            numlower: 2,
            lowerstack: Vec::new(),
        }
    }
}
#[derive(Debug)]
pub struct OvlPath {
    layer: Arc<OvlLayer>,
    inode: Arc<dyn IndexNode>,
}
#[derive(Debug)]
pub struct OvlLayer {
    pub mnt: Arc<OvlInode>, // 挂载点
    pub index: u32,         // 0 是上层读写层，>0 是下层只读层
    pub fsid: u32,          // 文件系统标识符
}
