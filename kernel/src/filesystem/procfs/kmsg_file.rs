//! /proc/kmsg - 内核消息缓冲区
//!
//! 这个文件提供对内核日志消息的访问

use crate::filesystem::{
    procfs::{
        kmsg::{LogSnapshot, SnapshotKind, KMSG},
        template::{Builder, FileOps, ProcFileBuilder},
    },
    vfs::{FilePrivateData, IndexNode, InodeMode},
};
use crate::libs::mutex::MutexGuard;
use alloc::sync::{Arc, Weak};
use system_error::SystemError;

/// /proc/kmsg 文件的 FileOps 实现
#[derive(Debug)]
pub struct KmsgFileOps;

impl KmsgFileOps {
    pub fn new_inode(parent: Weak<dyn IndexNode>) -> Arc<dyn IndexNode> {
        ProcFileBuilder::new(Self, InodeMode::S_IRUGO) // 0444 - 所有用户可读
            .parent(parent)
            .build()
            .unwrap()
    }
}

impl FileOps for KmsgFileOps {
    fn supports_read_iter_flags(&self) -> bool {
        false
    }

    fn read_at(
        &self,
        _offset: usize,
        _len: usize,
        buf: &mut [u8],
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        // 访问全局 KMSG 缓冲区
        let kmsg = unsafe { KMSG.as_ref().ok_or(SystemError::ENODEV)? };
        let mut snapshot = LogSnapshot::try_new()?;
        kmsg.lock_irqsave()
            .capture(&mut snapshot, SnapshotKind::Prefix);
        let mut offset = 0;
        snapshot.read_prefix(buf.len(), |bytes| {
            buf[offset..offset + bytes.len()].copy_from_slice(bytes);
            offset += bytes.len();
            Ok(())
        })
    }
}
