//! /proc/[pid]/mountstats - 进程挂载统计信息

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

#[derive(Debug)]
pub struct MountStatsFileOps {
    target: ProcPidTarget,
}

impl MountStatsFileOps {
    pub fn new_inode(target: ProcPidTarget, parent: Weak<dyn IndexNode>) -> Arc<dyn IndexNode> {
        ProcFileBuilder::new(Self { target }, InodeMode::S_IRUGO)
            .parent(parent)
            .build()
            .unwrap()
    }
}

impl FileOps for MountStatsFileOps {
    fn open(&self, data: &mut MutexGuard<FilePrivateData>) -> Result<(), SystemError> {
        open_mount_file_for_target(&self.target, ProcMountRenderKind::MountStats, data)
    }

    fn read_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        self.target
            .thread_group_leader()
            .ok_or(SystemError::ESRCH)?;
        read_cached_mount_file(offset, len, buf, data)
    }
}
