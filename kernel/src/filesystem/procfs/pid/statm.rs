//! /proc/[pid]/statm - 进程内存统计信息
//!
//! 显示进程的内存使用统计（以页为单位）

use crate::libs::mutex::MutexGuard;
use crate::{
    arch::MMArch,
    filesystem::{
        procfs::{
            pid::ProcPidTarget,
            template::{Builder, FileOps, ProcFileBuilder},
            utils::proc_read_snapshot,
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
    fn read_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        mut data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        // `single_open()`: one fd sees one statm record.
        proc_read_snapshot(offset, len, buf, &mut data, || {
            let pcb = self
                .target
                .thread_group_leader()
                .ok_or(SystemError::ESRCH)?;

            let user_vm = {
                let basic = pcb.basic();
                basic.user_vm()
            };

            // Process memory information (simplified: no shared/text/lib/data).
            let (size_pages, resident_pages) = user_vm
                .map(|vm| {
                    let guard = vm.read_guard_no_reservations();
                    // Field 1 is total virtual pages; field 2 uses the RAS-maintained resident page count.
                    let size_pages = (guard
                        .vma_usage_bytes()
                        .saturating_add(MMArch::PAGE_SIZE - 1))
                        >> MMArch::PAGE_SHIFT;
                    (size_pages, vm.resident_pages())
                })
                .unwrap_or((0, 0));

            // statm layout: size resident shared text lib data dt.
            // Only size/resident are implemented; the remaining fields stay 0.
            let content = format!("{} {} 0 0 0 0 0\n", size_pages, resident_pages);

            Ok(content.into_bytes())
        })
    }
}
