//! /proc/version - 内核版本信息
//!
//! 这个文件展示了内核版本、编译信息等

use crate::libs::mutex::MutexGuard;
use crate::{
    filesystem::{
        procfs::{
            template::{Builder, FileOps, ProcFileBuilder},
            utils::proc_read,
        },
        vfs::{FilePrivateData, IndexNode, InodeMode},
    },
    init::version_info,
};
use alloc::{
    borrow::ToOwned,
    format,
    sync::{Arc, Weak},
    vec::Vec,
};
use system_error::SystemError;

/// /proc/version 文件的 FileOps 实现
#[derive(Debug)]
pub struct VersionFileOps;

impl VersionFileOps {
    pub fn new_inode(parent: Weak<dyn IndexNode>) -> Arc<dyn IndexNode> {
        ProcFileBuilder::new(Self, InodeMode::S_IRUGO) // 0444 - 所有用户可读
            .parent(parent)
            .build()
            .unwrap()
    }

    fn generate_version_content() -> Vec<u8> {
        let info = version_info::get_kernel_build_info();

        // Linux version 5.15.0-152-generic (buildd@lcy02-amd64-094) (gcc (Ubuntu 11.4.0-1ubuntu1~22.04) 11.4.0, GNU ld (GNU Binutils for Ubuntu) 2.38) #162-Ubuntu SMP Wed Jul 23 09:48:42 UTC 2025
        let version_content = format!(
            "Linux version {} ({}@{}) ({}, {}) {}\n",
            info.release,
            info.build_user,
            info.build_host,
            info.compiler_info,
            info.linker_info,
            info.version
        );

        version_content.into_bytes().to_owned()
    }
}

impl FileOps for VersionFileOps {
    fn read_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        let content = Self::generate_version_content();
        proc_read(offset, len, buf, &content)
    }
}
