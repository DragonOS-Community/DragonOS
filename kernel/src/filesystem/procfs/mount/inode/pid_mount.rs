use core::fmt::Debug;

use crate::filesystem::{
    procfs::{
        mount::{open_mount_file_for_target, read_cached_mount_file, ProcMountRenderKind},
        pid::ProcPidTarget,
        template::{Builder, FileOps, ProcFileBuilder},
    },
    vfs::{FilePrivateData, IndexNode, InodeMode},
};
use crate::libs::mutex::MutexGuard;
use alloc::sync::{Arc, Weak};
use system_error::SystemError;

#[derive(Debug)]
pub struct MountProcFileOps {
    target: ProcPidTarget,
    kind: ProcMountRenderKind,
}

impl MountProcFileOps {
    pub fn new_inode(
        target: ProcPidTarget,
        kind: ProcMountRenderKind,
        parent: Weak<dyn IndexNode>,
    ) -> Arc<dyn IndexNode> {
        // Linux: mounts/mountinfo are world-readable; mountstats is owner-read only (0400).
        let mode = match kind {
            ProcMountRenderKind::MountStats => InodeMode::S_IRUSR,
            _ => InodeMode::S_IRUGO,
        };
        ProcFileBuilder::new(Self { target, kind }, mode)
            .parent(parent)
            .build()
            .unwrap()
    }
}

impl FileOps for MountProcFileOps {
    fn owner(&self) -> Option<(usize, usize)> {
        self.target.owner_uid_gid()
    }

    fn open(&self, data: &mut MutexGuard<FilePrivateData>) -> Result<(), SystemError> {
        open_mount_file_for_target(&self.target, self.kind, data)
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
