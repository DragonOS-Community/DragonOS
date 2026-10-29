//! /proc/[pid]/cmdline - 进程命令行参数
//!
//! 返回进程的完整命令行，各参数之间以 \0 分隔

use crate::filesystem::vfs::{file::FileMode, OpenFileBehavior};
use crate::filesystem::{
    procfs::{
        pid::ProcPidTarget,
        template::{Builder, FileOps, ProcFileBuilder},
        utils::proc_read,
    },
    vfs::{FilePrivateData, IndexNode, InodeMode},
};
use crate::libs::mutex::MutexGuard;
use alloc::sync::{Arc, Weak};
use system_error::SystemError;

/// /proc/[pid]/cmdline 文件的 FileOps 实现
#[derive(Debug)]
pub struct CmdlineFileOps {
    target: ProcPidTarget,
}

impl CmdlineFileOps {
    pub fn new_inode(target: ProcPidTarget, parent: Weak<dyn IndexNode>) -> Arc<dyn IndexNode> {
        ProcFileBuilder::new(Self { target }, InodeMode::S_IRUGO)
            .parent(parent)
            .build()
            .unwrap()
    }
}

impl FileOps for CmdlineFileOps {
    fn configure_open_file(&self, _data: &FilePrivateData, behavior: &mut OpenFileBehavior) {
        behavior.mode.remove(FileMode::FMODE_CAN_WRITE);
    }

    fn supports_read_iter_flags(&self) -> bool {
        false
    }

    fn read_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        let pcb = self.target.task().ok_or(SystemError::ESRCH)?;

        // Clone only the immutable owner, then release the PCB guard before
        // borrowing its page chunks. Partial reads allocate no full-size copy.
        if let Some(snapshot) = pcb.cmdline_snapshot() {
            if snapshot.byte_len() != 0 {
                let count = len.min(buf.len());
                return Ok(snapshot.copy_range(offset, &mut buf[..count]));
            }
        }
        let mut content = {
            let basic = pcb.basic();
            basic.name().as_bytes().to_vec()
        };
        content.push(0);

        proc_read(offset, len, buf, &content)
    }
}
