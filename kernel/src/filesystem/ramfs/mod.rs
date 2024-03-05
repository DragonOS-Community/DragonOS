mod entry;

use core::any::Any;

use alloc::sync::Arc;

use self::entry::LockedEntry;

use super::vfs::{FileSystem, FsInfo};

/// RamFS的inode名称的最大长度
const RAMFS_MAX_NAMELEN: usize = 64;

#[derive(Debug)]
pub struct RamFS {
    root: Arc<LockedEntry>,
    // To Add Cache
}


impl FileSystem for RamFS {
    fn root_inode(&self) -> Arc<dyn super::vfs::IndexNode> {
        self.root.clone()
    }

    fn info(&self) -> FsInfo {
        FsInfo {
            blk_dev_id: 0,
            max_name_len: RAMFS_MAX_NAMELEN,
        }
    }

    /// @brief 本函数用于实现动态转换。
    /// 具体的文件系统在实现本函数时，最简单的方式就是：直接返回self
    fn as_any_ref(&self) -> &dyn Any {
        self
    }
}
