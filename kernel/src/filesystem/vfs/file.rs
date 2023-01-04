use alloc::sync::Arc;

use super::IndexNode;

/// @brief 抽象文件结构体
pub struct File {
    inode: Arc<dyn IndexNode>,
    offset: usize,
    mode: u32,
}

impl File{
    // TODO
}
