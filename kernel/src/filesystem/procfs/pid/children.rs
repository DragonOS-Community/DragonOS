//! /proc/<pid>/task/<tid>/children: the named thread's natural children.

use alloc::{
    format,
    sync::{Arc, Weak},
};
use system_error::SystemError;

use crate::{
    filesystem::{
        procfs::{
            pid::ProcPidTarget,
            template::{Builder, FileOps, ProcFileBuilder},
            utils::proc_read_seq,
        },
        vfs::{FilePrivateData, IndexNode, InodeMode},
    },
    libs::mutex::MutexGuard,
};

#[derive(Debug)]
pub struct ChildrenFileOps {
    target: ProcPidTarget,
}

impl ChildrenFileOps {
    pub fn new_inode(target: ProcPidTarget, parent: Weak<dyn IndexNode>) -> Arc<dyn IndexNode> {
        ProcFileBuilder::new(Self { target }, InodeMode::S_IRUGO)
            .parent(parent)
            .build()
            .unwrap()
    }
}

impl FileOps for ChildrenFileOps {
    fn read_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        mut data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        // Linux 6.6 fs/proc/array.c uses a seq iterator, not a per-read string.
        // Keep records buffered across short reads and support rewind/seek.
        proc_read_seq(offset, len, buf, &mut data, |cursor, _budget, out| {
            let Some(parent) = self.target.task() else {
                // get_children_pid() returns NULL once the named task is gone.
                return Ok(None);
            };
            // Bound both pinned PIDs and rendered bytes (at most 64 u32 IDs).
            const SLICE_CHILDREN: usize = 64;
            let start = cursor.unwrap_or(0);
            let children = parent.child_pids_range(start, SLICE_CHILDREN)?;
            for pid in &children {
                // IDs belong to the proc mount's namespace, not the reader's.
                let nr = pid.pid_nr_ns(self.target.view_pid_ns());
                out.extend_from_slice(format!("{} ", nr.data()).as_bytes());
            }
            // Like Linux's positional fallback, concurrent removal can skip a
            // child between slices. No atomic tree snapshot is promised.
            Ok((children.len() == SLICE_CHILDREN).then_some(start + children.len()))
        })
    }
}
