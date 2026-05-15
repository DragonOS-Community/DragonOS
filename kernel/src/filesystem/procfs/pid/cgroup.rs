//! /proc/[pid]/cgroup - cgroup membership (v2)

use crate::libs::mutex::MutexGuard;
use crate::{
    cgroup::cgroup_path_from_view,
    filesystem::{
        procfs::{
            template::{Builder, FileOps, ProcFileBuilder},
            utils::proc_read,
        },
        vfs::{FilePrivateData, IndexNode, InodeMode},
    },
    process::{ProcessManager, RawPid},
};
use alloc::{
    format,
    sync::{Arc, Weak},
    vec::Vec,
};
use system_error::SystemError;

#[derive(Debug)]
pub struct CgroupFileOps {
    pid: RawPid,
}

impl CgroupFileOps {
    pub fn new_inode(pid: RawPid, parent: Weak<dyn IndexNode>) -> Arc<dyn IndexNode> {
        ProcFileBuilder::new(Self { pid }, InodeMode::S_IRUGO)
            .parent(parent)
            .build()
            .unwrap()
    }

    fn generate_content(&self) -> Result<Vec<u8>, SystemError> {
        let target = ProcessManager::find(self.pid).ok_or(SystemError::ESRCH)?;
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
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        let content = self.generate_content()?;
        proc_read(offset, len, buf, &content)
    }
}
