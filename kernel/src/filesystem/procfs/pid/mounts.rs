//! /proc/[pid]/mounts - 进程挂载点信息
//!
//! 显示进程的挂载点信息

use crate::libs::mutex::MutexGuard;
use crate::{
    filesystem::{
        procfs::{
            mount_view::{open_pid_mount_file, read_cached_mount_file, ProcMountRenderKind},
            template::{Builder, FileOps, ProcFileBuilder},
        },
        vfs::{file::FileFlags, FilePrivateData, IndexNode, InodeMode},
    },
    process::{ProcessManager, RawPid},
};
use alloc::sync::{Arc, Weak};
use system_error::SystemError;

/// /proc/[pid]/mounts 文件的 FileOps 实现
#[derive(Debug)]
pub struct PidMountsFileOps {
    #[allow(dead_code)]
    pid: RawPid,
}

impl PidMountsFileOps {
    pub fn new_inode(pid: RawPid, parent: Weak<dyn IndexNode>) -> Arc<dyn IndexNode> {
        ProcFileBuilder::new(Self { pid }, InodeMode::S_IRUGO)
            .parent(parent)
            .build()
            .unwrap()
    }
}

impl FileOps for PidMountsFileOps {
    fn open(&self, data: &mut FilePrivateData, _flags: &FileFlags) -> Result<(), SystemError> {
        open_pid_mount_file(self.pid, ProcMountRenderKind::Mounts, data)
    }

    fn read_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        read_cached_mount_file(offset, len, buf, data)
    }

    fn owner(&self) -> Option<(usize, usize)> {
        let pcb = ProcessManager::find(self.pid)?;
        if pcb.is_kthread() {
            return Some((0, 0));
        }
        let cred = pcb.cred();
        Some((cred.euid.data(), cred.egid.data()))
    }
}
