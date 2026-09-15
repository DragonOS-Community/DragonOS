//! /proc/[pid]/cgroup - cgroup membership (v2)

use crate::libs::mutex::MutexGuard;
use crate::{
    cgroup::cgroup_path_from_view,
    filesystem::{
        procfs::{
            pid::ProcPidTarget,
            template::{Builder, FileOps, ProcFileBuilder},
            utils::proc_read_snapshot,
        },
        vfs::{FilePrivateData, IndexNode, InodeMode},
    },
    process::ProcessManager,
};
use alloc::{
    format,
    sync::{Arc, Weak},
    vec::Vec,
};
use system_error::SystemError;

#[derive(Debug)]
pub struct CgroupFileOps {
    target: ProcPidTarget,
}

impl CgroupFileOps {
    pub fn new_inode(target: ProcPidTarget, parent: Weak<dyn IndexNode>) -> Arc<dyn IndexNode> {
        ProcFileBuilder::new(Self { target }, InodeMode::S_IRUGO)
            .parent(parent)
            .build()
            .unwrap()
    }

    fn generate_content(&self) -> Result<Vec<u8>, SystemError> {
        let target = self
            .target
            .thread_group_leader()
            .ok_or(SystemError::ESRCH)?;
        let viewer = ProcessManager::current_pcb();

        let target_cg = target.task_cgroup_node();
        let ns_root = viewer.nsproxy().cgroup_ns.root_cgroup().clone();
        let rel = cgroup_path_from_view(&target_cg, &ns_root);

        Ok(format!("0::{}\n", rel).into_bytes())
    }
}

impl FileOps for CgroupFileOps {
    fn read_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        mut data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        // `single_open()`: one fd sees one cgroup record.
        proc_read_snapshot(offset, len, buf, &mut data, || self.generate_content())
    }
}
