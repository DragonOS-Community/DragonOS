//! /proc/[pid]/statm - 进程内存统计信息
//!
//! 显示进程的内存使用统计（以页为单位）

use crate::libs::mutex::MutexGuard;
use crate::{
    arch::MMArch,
    filesystem::{
        procfs::{
            template::{Builder, FileOps, ProcFileBuilder},
            utils::proc_read,
        },
        vfs::{FilePrivateData, IndexNode, InodeMode},
    },
    mm::MemoryManagementArch,
    process::{ProcessManager, RawPid},
};
use alloc::{
    format,
    sync::{Arc, Weak},
};
use system_error::SystemError;

/// /proc/[pid]/statm 文件的 FileOps 实现
#[derive(Debug)]
pub struct StatmFileOps {
    pid: RawPid,
}

impl StatmFileOps {
    pub fn new_inode(pid: RawPid, parent: Weak<dyn IndexNode>) -> Arc<dyn IndexNode> {
        ProcFileBuilder::new(Self { pid }, InodeMode::S_IRUGO)
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
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        // 查找进程
        let pcb = ProcessManager::find(self.pid).ok_or(SystemError::ESRCH)?;

        // 获取进程内存信息（简化实现）
        let size_pages = pcb
            .basic()
            .user_vm()
            .map(|vm| {
                let guard = vm.read();
                // statm 第一列为总虚拟内存页数
                (guard
                    .vma_usage_bytes()
                    .saturating_add(MMArch::PAGE_SIZE - 1))
                    >> MMArch::PAGE_SHIFT
            })
            .unwrap_or(0);

        // statm 格式: size resident shared text lib data dt
        // 简化实现，只返回 size，其他字段为 0
        let content = format!("{} 0 0 0 0 0 0\n", size_pages);

        proc_read(offset, len, buf, content.as_bytes())
    }
}
