use core::fmt::Debug;

use crate::filesystem::{
    procfs::{
        mount::{render_mount_slice, MountView, ProcMountRenderKind},
        pid::ProcPidTarget,
        template::{Builder, FileOps, ProcFileBuilder},
        utils::proc_read_seq,
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

    fn open(
        &self,
        data: &mut MutexGuard<FilePrivateData>,
        _flags: &crate::filesystem::vfs::file::FileFlags,
    ) -> Result<(), SystemError> {
        // Linux `mounts_open_common()` resolves `get_proc_task(inode)` once at
        // open time and keeps its mount namespace and root path in the seq
        // private data, so a `setns()`, `unshare()` or `chroot()` performed
        // afterwards cannot change what this fd reports. The records are
        // produced by the reads, like any other `seq_file`.
        //
        // The task is the one this node names, not the group leader: a thread
        // can unshare its mount namespace or its `fs_struct`, and Linux then
        // reports that thread's view. For a `/proc/<tgid>` node both are the
        // leader.
        let task = self.target.task().ok_or(SystemError::EINVAL)?;
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
        // One slice is one output block, so an fd *keeps* a page of the table
        // rather than a copy of all of it (a container may hold up to
        // `mount-max` mounts and the reader up to `RLIMIT_NOFILE` fds); a slice
        // still walks the namespace's mounts to find the ones that follow the
        // cursor, which is work rather than retention (see
        // `render_mount_slice()`). The cursor is the mount id the slice reached,
        // so a mount created after an earlier slice is reported by a later one,
        // the way `seq_read_iter()` re-enters `show()` per record, while a mount
        // the topology took out of the pinned root in between is not reported at
        // all.
        proc_read_seq(offset, len, buf, &mut data, |cursor, budget, out| {
            render_mount_slice(&view, self.kind, cursor, budget, out)
        })
    }
}
