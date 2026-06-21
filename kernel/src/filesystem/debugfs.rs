use core::any::Any;

use alloc::sync::Arc;
use system_error::SystemError;

use crate::debug::sysfs::debugfs_kobj;
use crate::driver::base::kobject::KObject;
use crate::filesystem::vfs::{
    FileSystem, FileSystemMakerData, FsInfo, IndexNode, Magic, MountableFileSystem, SuperBlock,
    FSMAKER,
};
use crate::register_mountable_fs;

use linkme::distributed_slice;

const DEBUGFS_MAX_NAMELEN: u64 = 255;
const DEBUGFS_BLOCK_SIZE: u64 = 4096;

#[derive(Debug)]
pub struct DebugFs {
    root: Arc<dyn IndexNode>,
}

impl DebugFs {
    fn new() -> Result<Arc<Self>, SystemError> {
        let root = debugfs_kobj().inode().ok_or(SystemError::ENOENT)?;
        Ok(Arc::new(Self { root }))
    }
}

impl FileSystem for DebugFs {
    fn root_inode(&self) -> Arc<dyn IndexNode> {
        self.root.clone()
    }

    fn info(&self) -> FsInfo {
        FsInfo {
            blk_dev_id: 0,
            max_name_len: DEBUGFS_MAX_NAMELEN as usize,
        }
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }

    fn name(&self) -> &str {
        "debugfs"
    }

    fn super_block(&self) -> SuperBlock {
        SuperBlock::new(
            Magic::DEBUGFS_MAGIC,
            DEBUGFS_BLOCK_SIZE,
            DEBUGFS_MAX_NAMELEN,
        )
    }
}

impl MountableFileSystem for DebugFs {
    fn make_mount_data(
        _raw_data: Option<&str>,
        _source: &str,
    ) -> Result<Option<Arc<dyn FileSystemMakerData + 'static>>, SystemError> {
        Ok(None)
    }

    fn make_fs(
        _data: Option<&dyn FileSystemMakerData>,
    ) -> Result<Arc<dyn FileSystem + 'static>, SystemError> {
        Ok(Self::new()?)
    }
}

register_mountable_fs!(DebugFs, DEBUGFSMAKER, "debugfs");
