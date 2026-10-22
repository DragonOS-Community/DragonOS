//! /proc/version_signature - 内核版本签名
//!
//! 这个文件用于兼容 Aya 框架的内核版本识别
//! 格式: DragonOS 6.0.0-generic 6.0.0

use crate::filesystem::procfs::template::configure_seq_file;
use crate::filesystem::vfs::OpenFileBehavior;
use crate::filesystem::{
    procfs::{
        template::{Builder, FileOps, ProcFileBuilder},
        utils::proc_snapshot_operation,
    },
    vfs::{FilePrivateData, IndexNode, InodeMode},
};
use crate::libs::mutex::MutexGuard;
use alloc::sync::{Arc, Weak};
use system_error::SystemError;

/// /proc/version_signature 文件的 FileOps 实现
#[derive(Debug)]
pub struct VersionSignatureFileOps;

impl VersionSignatureFileOps {
    pub fn new_inode(parent: Weak<dyn IndexNode>) -> Arc<dyn IndexNode> {
        ProcFileBuilder::new(Self, InodeMode::S_IRUGO)
            .parent(parent)
            .build()
            .unwrap()
    }

    const VERSION_SIGNATURE: &'static [u8] = b"DragonOS 6.0.0-generic 6.0.0\n";
}

impl FileOps for VersionSignatureFileOps {
    fn configure_open_file(&self, _data: &FilePrivateData, behavior: &mut OpenFileBehavior) {
        configure_seq_file(behavior);
    }

    const SEEK_KIND: crate::filesystem::procfs::template::ProcfsSeekKind =
        crate::filesystem::procfs::template::ProcfsSeekKind::Seq;

    fn seq_operation(
        &self,
        operation: crate::filesystem::procfs::utils::ProcfsSeqOperation<'_>,
        data: &mut MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        // Like the other one-record procfs files, this keeps a single record per
        // fd, so a later read cannot observe a different version string.
        proc_snapshot_operation(operation, data, || Ok(Self::VERSION_SIGNATURE.to_vec()))
    }
}
