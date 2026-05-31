//! /proc/[pid]/mounts - 进程挂载点信息
//!
//! 显示进程的挂载点信息

use crate::filesystem::{
    procfs::{
        mount_view::{open_mount_file_for_target, read_cached_mount_file, ProcMountRenderKind},
        pid::ProcPidTarget,
        template::{Builder, FileOps, ProcFileBuilder},
    },
    vfs::{FilePrivateData, IndexNode, InodeMode},
};
use crate::libs::mutex::MutexGuard;
use alloc::sync::{Arc, Weak};
use system_error::SystemError;

/// /proc/[pid]/mounts 文件的 FileOps 实现
#[derive(Debug)]
pub struct PidMountsFileOps {
    target: ProcPidTarget,
}

impl PidMountsFileOps {
    pub fn new_inode(target: ProcPidTarget, parent: Weak<dyn IndexNode>) -> Arc<dyn IndexNode> {
        ProcFileBuilder::new(Self { target }, InodeMode::S_IRUGO)
            .parent(parent)
            .build()
            .unwrap()
    }
}

impl FileOps for PidMountsFileOps {
    fn open(&self, data: &mut MutexGuard<FilePrivateData>) -> Result<(), SystemError> {
        open_mount_file_for_target(&self.target, ProcMountRenderKind::Mounts, data)
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

    fn owner(&self) -> Option<(usize, usize)> {
        self.target.owner_uid_gid()
    }
}
