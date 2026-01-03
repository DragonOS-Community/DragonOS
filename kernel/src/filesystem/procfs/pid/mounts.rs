//! /proc/[pid]/mounts - 进程挂载点信息
//!
//! 显示进程的挂载点信息

use crate::{
    filesystem::{
        procfs::{
            mounts::generate_mounts_content,
            template::{Builder, FileOps, ProcFileBuilder},
            utils::proc_read,
        },
        vfs::{FilePrivateData, IndexNode, InodeMode},
    },
    libs::spinlock::SpinLockGuard,
    process::RawPid,
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
    fn read_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        _data: SpinLockGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        let content = generate_mounts_content();
        proc_read(offset, len, buf, content.as_bytes())
    }
}
