use core::fmt::Debug;

use crate::filesystem::{
    procfs::{
        mount::{render_mount_file_for_task, ProcMountRenderKind},
        pid::ProcPidTarget,
        template::{Builder, FileOps, ProcFileBuilder},
        utils::proc_read_snapshot,
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

    fn open(&self, _data: &mut MutexGuard<FilePrivateData>) -> Result<(), SystemError> {
        // Linux `mounts_open_common()` resolves the target with `get_proc_task()`
        // at open time and fails with `ESRCH` when it is already gone. The record
        // itself is rendered on the first read, like any other `seq_file`.
        self.target.thread_group_leader().ok_or(SystemError::ESRCH)?;
        Ok(())
    }

    fn read_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        mut data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        // The target is resolved by the renderer, never on a continuation read.
        // `seq_read_iter()` does not re-enter a handler while its buffer still
        // holds data, so a reader that already took the first chunk keeps
        // draining this fd's snapshot even after the thread group is gone.
        proc_read_snapshot(offset, len, buf, &mut data, || {
            let task = self
                .target
                .thread_group_leader()
                .ok_or(SystemError::ESRCH)?;
            render_mount_file_for_task(&task, self.kind)
        })
    }
}
