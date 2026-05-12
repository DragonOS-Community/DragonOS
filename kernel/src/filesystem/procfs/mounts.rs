//! /proc/mounts - 当前进程视角的挂载点信息

use crate::filesystem::{
    procfs::{
        mount_view::{open_current_mount_file, read_cached_mount_file, ProcMountRenderKind},
        template::{Builder, FileOps, ProcFileBuilder},
    },
    vfs::{file::FileFlags, FilePrivateData, IndexNode, InodeMode},
};
use crate::libs::mutex::MutexGuard;
use alloc::sync::{Arc, Weak};
use system_error::SystemError;

/// /proc/mounts 文件的 FileOps 实现
#[derive(Debug)]
pub struct MountsFileOps;

impl MountsFileOps {
    pub fn new_inode(parent: Weak<dyn IndexNode>) -> Arc<dyn IndexNode> {
        ProcFileBuilder::new(Self, InodeMode::S_IRUGO) // 0444 - 所有用户可读
            .parent(parent)
            .build()
            .unwrap()
    }
}

impl FileOps for MountsFileOps {
    fn open(&self, data: &mut FilePrivateData, _flags: &FileFlags) -> Result<(), SystemError> {
        open_current_mount_file(ProcMountRenderKind::Mounts, data)
    }

    fn read_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        read_cached_mount_file(offset, len, buf, data)
    }
}
