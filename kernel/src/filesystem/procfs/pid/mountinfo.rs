//! /proc/[pid]/mountinfo - 进程挂载点详细信息
//!
//! 显示进程的挂载点详细信息

use crate::filesystem::{
    procfs::{
        mounts::{
            cache_procfs_file_content, generate_mountinfo_content_for_task,
            read_cached_procfs_file_content,
        },
        pid::ProcPidTarget,
        template::{Builder, FileOps, ProcFileBuilder},
    },
    vfs::{FilePrivateData, IndexNode, InodeMode},
};
use crate::libs::mutex::MutexGuard;
use alloc::sync::{Arc, Weak};
use system_error::SystemError;

/// /proc/[pid]/mountinfo 文件的 FileOps 实现
#[derive(Debug)]
pub struct MountInfoFileOps {
    target: ProcPidTarget,
}

impl MountInfoFileOps {
    pub fn new_inode(target: ProcPidTarget, parent: Weak<dyn IndexNode>) -> Arc<dyn IndexNode> {
        ProcFileBuilder::new(Self { target }, InodeMode::S_IRUGO)
            .parent(parent)
            .build()
            .unwrap()
    }
}

impl FileOps for MountInfoFileOps {
    fn open(&self, data: &mut MutexGuard<FilePrivateData>) -> Result<(), SystemError> {
        let target = self
            .target
            .thread_group_leader()
            .ok_or(SystemError::ESRCH)?;
        cache_procfs_file_content(data, generate_mountinfo_content_for_task(&target))
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
        read_cached_procfs_file_content(offset, len, buf, data)
    }
}
