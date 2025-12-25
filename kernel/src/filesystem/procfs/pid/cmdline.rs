//! /proc/[pid]/cmdline - 进程命令行参数
//!
//! 返回进程的完整命令行，各参数之间以 \0 分隔

use crate::{
    filesystem::{
        procfs::{
            template::{Builder, FileOps, ProcFileBuilder},
            utils::proc_read,
        },
        vfs::{FilePrivateData, IndexNode, InodeMode},
    },
    libs::spinlock::SpinLockGuard,
    process::{ProcessManager, RawPid},
};
use alloc::sync::{Arc, Weak};
use system_error::SystemError;

/// /proc/[pid]/cmdline 文件的 FileOps 实现
#[derive(Debug)]
pub struct CmdlineFileOps {
    pid: RawPid,
}

impl CmdlineFileOps {
    pub fn new_inode(pid: RawPid, parent: Weak<dyn IndexNode>) -> Arc<dyn IndexNode> {
        ProcFileBuilder::new(Self { pid }, InodeMode::S_IRUGO)
            .parent(parent)
            .build()
            .unwrap()
    }
}

impl FileOps for CmdlineFileOps {
    fn read_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        _data: SpinLockGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        // 查找进程
        let pcb = ProcessManager::find(self.pid).ok_or(SystemError::ESRCH)?;

        // 获取 cmdline 字节
        let cmdline_bytes = pcb.cmdline_bytes();

        // 如果 cmdline 为空，返回进程名
        let mut content = if cmdline_bytes.is_empty() {
            let name = pcb.basic().name().as_bytes().to_vec();
            let mut result = name;
            result.push(0); // 以 \0 结尾
            result
        } else {
            cmdline_bytes
        };

        if !content.ends_with(b"\n") {
            content.push(b'\n');
        }

        proc_read(offset, len, buf, &content)
    }
}
