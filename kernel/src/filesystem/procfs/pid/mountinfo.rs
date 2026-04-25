//! /proc/[pid]/mountinfo - 进程挂载点详细信息
//!
//! 显示进程的挂载点详细信息

use crate::libs::mutex::MutexGuard;
use crate::{
    filesystem::{
        procfs::{
            mounts::generate_mountinfo_content,
            template::{Builder, FileOps, ProcFileBuilder},
            utils::proc_read,
        },
        vfs::{FilePrivateData, IndexNode, InodeMode},
    },
    process::RawPid,
};
use alloc::sync::{Arc, Weak};
use system_error::SystemError;

/// /proc/[pid]/mountinfo 文件的 FileOps 实现
#[derive(Debug)]
pub struct MountInfoFileOps {
    #[allow(dead_code)]
    pid: RawPid,
}

impl MountInfoFileOps {
    pub fn new_inode(pid: RawPid, parent: Weak<dyn IndexNode>) -> Arc<dyn IndexNode> {
        ProcFileBuilder::new(Self { pid }, InodeMode::S_IRUGO)
            .parent(parent)
            .build()
            .unwrap()
    }
}

impl FileOps for MountInfoFileOps {
    fn read_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        let content = generate_mountinfo_content();
        proc_read(offset, len, buf, content.as_bytes())
    }
}
