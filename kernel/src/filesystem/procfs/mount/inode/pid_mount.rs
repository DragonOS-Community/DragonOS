use core::fmt::Debug;

use crate::filesystem::{
    procfs::{
        mount::{render_mount_file, MountView, ProcMountRenderKind},
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

    fn open(&self, data: &mut MutexGuard<FilePrivateData>) -> Result<(), SystemError> {
        // Linux `mounts_open_common()` resolves the target once at open time and
        // keeps its mount namespace and root path in the seq private data, so a
        // `setns()`, `unshare()` or `chroot()` performed afterwards cannot
        // change what this fd reports. The record itself is rendered on the
        // first read, like any other `seq_file`.
        let task = self
            .target
            .thread_group_leader()
            .ok_or(SystemError::ESRCH)?;
        let view = MountView::capture(&task)?;
        let FilePrivateData::Procfs(pdata) = &mut **data else {
            return Err(SystemError::EINVAL);
        };
        pdata.mount_view = Some(view);
        Ok(())
    }

    fn read_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        mut data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        // The view is taken once, by `open()`: rendering resolves neither the
        // target nor its root again, so a reader that already took the first
        // chunk keeps draining this fd's record even after the thread group is
        // gone, and a root change after open() does not move an open fd. A read
        // without that state did not come through the procfs `open()` hook and
        // has no view to render from.
        let view = {
            let FilePrivateData::Procfs(pdata) = &*data else {
                return Err(SystemError::EINVAL);
            };
            pdata.mount_view.clone().ok_or(SystemError::EINVAL)?
        };
        proc_read_snapshot(offset, len, buf, &mut data, move || {
            render_mount_file(&view, self.kind)
        })
    }
}
