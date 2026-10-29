//! /proc/[pid]/cgroup - cgroup membership (v2)

use crate::filesystem::procfs::template::configure_readonly_seq_file;
use crate::filesystem::vfs::OpenFileBehavior;
use crate::libs::mutex::MutexGuard;
use crate::{
    cgroup::cgroup_path_from_view,
    filesystem::{
        procfs::{
            pid::ProcPidTarget,
            template::{Builder, FileOps, ProcFileBuilder},
            utils::proc_snapshot_operation,
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
        // Linux `proc_cgroup_show()` runs on `get_proc_task(inode)`, so a
        // hidden tid and /proc/<tgid>/task/<tid>/cgroup report the leaf of the
        // thread they name, even when cgroup.threads splits a thread group.
        let target = self.target.task().ok_or(SystemError::ESRCH)?;
        let viewer = ProcessManager::current_pcb();

        let target_cg = target.task_cgroup_node();
        let ns_root = viewer.nsproxy().cgroup_ns.root_cgroup().clone();
        let rel = cgroup_path_from_view(&target_cg, &ns_root);

        Ok(format!("0::{}\n", rel).into_bytes())
    }
}

impl FileOps for CgroupFileOps {
    fn configure_open_file(&self, _data: &FilePrivateData, behavior: &mut OpenFileBehavior) {
        configure_readonly_seq_file(behavior);
    }

    fn supports_read_iter_flags(&self) -> bool {
        false
    }

    const SEEK_KIND: crate::filesystem::procfs::template::ProcfsSeekKind =
        crate::filesystem::procfs::template::ProcfsSeekKind::Seq;

    fn seq_operation(
        &self,
        operation: crate::filesystem::procfs::utils::ProcfsSeqOperation<'_>,
        data: &mut MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        // `single_open()`: one fd sees one cgroup record.
        proc_snapshot_operation(operation, data, || self.generate_content())
    }
}
