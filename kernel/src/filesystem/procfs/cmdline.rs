//! /proc/cmdline - 内核启动命令行参数
//!
//! 这个文件展示了传递给内核的启动参数

use crate::filesystem::{
    procfs::{
        template::{Builder, FileOps, ProcFileBuilder},
        utils::proc_read,
    },
    vfs::{syscall::ModeType, FilePrivateData, IndexNode},
};
use alloc::{
    borrow::ToOwned,
    sync::{Arc, Weak},
    vec::Vec,
};
use system_error::SystemError;

/// /proc/cmdline 文件的 FileOps 实现
#[derive(Debug)]
pub struct CmdlineFileOps;

impl CmdlineFileOps {
    pub fn new_inode(parent: Weak<dyn IndexNode>) -> Arc<dyn IndexNode> {
        ProcFileBuilder::new(Self, ModeType::S_IRUGO) // 0444 - 所有用户可读
            .parent(parent)
            .build()
            .unwrap()
    }

    fn generate_cmdline_content() -> Vec<u8> {
        // TODO: 从 bootloader 获取实际的 cmdline
        // 目前返回一个占位符
        let cmdline = "BOOT_IMAGE=/boot/vmlinuz root=/dev/sda1 ro quiet splash\n";
        cmdline.as_bytes().to_owned()
    }
}

impl FileOps for CmdlineFileOps {
    fn read_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        _data: crate::libs::spinlock::SpinLockGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        let content = Self::generate_cmdline_content();

        proc_read(offset, len, buf, &content)
    }
}
