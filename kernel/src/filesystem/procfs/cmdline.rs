//! /proc/cmdline - 内核启动命令行参数
//!
//! 这个文件展示了传递给内核的启动参数

use crate::filesystem::procfs::template::configure_seq_file;
use crate::filesystem::vfs::OpenFileBehavior;
use alloc::string::ToString;

use crate::libs::mutex::MutexGuard;
use crate::{
    filesystem::{
        procfs::{
            template::{Builder, FileOps, ProcFileBuilder},
            utils::proc_snapshot_operation,
        },
        vfs::{FilePrivateData, IndexNode, InodeMode},
    },
    init::boot_params,
};
use alloc::{
    sync::{Arc, Weak},
    vec::Vec,
};
use system_error::SystemError;

/// /proc/cmdline 文件的 FileOps 实现
#[derive(Debug)]
pub struct CmdlineFileOps;

impl CmdlineFileOps {
    pub fn new_inode(parent: Weak<dyn IndexNode>) -> Arc<dyn IndexNode> {
        ProcFileBuilder::new(Self, InodeMode::S_IRUGO) // 0444 - 所有用户可读
            .parent(parent)
            .build()
            .unwrap()
    }

    fn generate_cmdline_content() -> Vec<u8> {
        let mut cmdline = boot_params()
            .read()
            .boot_cmdline_str()
            .to_string()
            .into_bytes();
        if !cmdline.ends_with(b"\n") {
            cmdline.push(b'\n');
        }

        cmdline
    }
}

impl FileOps for CmdlineFileOps {
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
        // `single_open()`: one fd sees one kernel command line record.
        proc_snapshot_operation(operation, data, || Ok(Self::generate_cmdline_content()))
    }
}
