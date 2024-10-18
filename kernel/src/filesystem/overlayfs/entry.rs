use alloc::sync::Arc;

use alloc::vec::Vec;

use crate::filesystem::vfs::IndexNode;

use super::OvlSuperBlock;
#[derive(Debug)]
pub struct OvlEntry {
    numlower: usize,
    lowerstack: Vec<OvlPath>,
}
#[derive(Debug)]
pub struct OvlPath {
    layer: Arc<OvlLayer>,
    inode: Arc<dyn IndexNode>,
}
#[derive(Debug)]
pub struct OvlLayer {
    pub mnt: Arc<dyn IndexNode>, // 挂载点
    pub fs: OvlSuperBlock,
    pub index: u32, // 0 是上层读写层，>0 是下层只读层
    pub fsid: u32,  // 文件系统标识符
}
