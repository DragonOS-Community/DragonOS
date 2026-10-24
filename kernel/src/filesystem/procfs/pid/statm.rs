//! /proc/[pid]/statm - 进程内存统计信息
//!
//! 显示进程的内存使用统计（以页为单位）

use crate::filesystem::procfs::template::configure_readonly_seq_file;
use crate::filesystem::vfs::OpenFileBehavior;
use crate::libs::mutex::MutexGuard;
use crate::{
    arch::MMArch,
    filesystem::{
        procfs::{
            pid::ProcPidTarget,
            template::{Builder, FileOps, ProcFileBuilder},
            utils::proc_snapshot_operation,
        },
        vfs::{FilePrivateData, IndexNode, InodeMode},
    },
    mm::MemoryManagementArch,
};
use alloc::{
    format,
    sync::{Arc, Weak},
};
use system_error::SystemError;

/// /proc/[pid]/statm 文件的 FileOps 实现
#[derive(Debug)]
pub struct StatmFileOps {
    target: ProcPidTarget,
}

impl StatmFileOps {
    pub fn new_inode(target: ProcPidTarget, parent: Weak<dyn IndexNode>) -> Arc<dyn IndexNode> {
        ProcFileBuilder::new(Self { target }, InodeMode::S_IRUGO)
            .parent(parent)
            .build()
            .unwrap()
    }
}

impl FileOps for StatmFileOps {
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
        // `single_open()`: one fd sees one statm record.
        proc_snapshot_operation(operation, data, || {
            let pcb = self.target.task().ok_or(SystemError::ESRCH)?;

            let user_vm = pcb.active_vm();

            // Process memory information (simplified: no shared/text/lib/data).
            let (size_pages, resident_pages) = user_vm
                .map(|mm| {
                    let vm = mm.vm();
                    let guard = vm.read_guard_no_reservations();
                    // Field 1 is total virtual pages; field 2 uses the RAS-maintained resident page count.
                    let size_pages = (guard
                        .vma_usage_bytes()
                        .saturating_add(MMArch::PAGE_SIZE - 1))
                        >> MMArch::PAGE_SHIFT;
                    let resident_pages = vm.resident_pages();
                    drop(guard);
                    // Never let final mmput re-enter the write lock under a read guard.
                    drop(mm);
                    (size_pages, resident_pages)
                })
                .unwrap_or((0, 0));

            // statm layout: size resident shared text lib data dt.
            // Only size/resident are implemented; the remaining fields stay 0.
            let content = format!("{} {} 0 0 0 0 0\n", size_pages, resident_pages);

            Ok(content.into_bytes())
        })
    }
}
