//! /proc/[pid]/exe - 进程可执行文件的符号链接
//!
//! 这个符号链接指向进程的可执行文件路径

use crate::{
    filesystem::{
        procfs::template::{Builder, ProcSymBuilder, SymOps},
        vfs::{syscall::ModeType, IndexNode},
    },
    process::{ProcessManager, RawPid},
};
use alloc::sync::{Arc, Weak};
use system_error::SystemError;

/// /proc/[pid]/exe 符号链接的 SymOps 实现
#[derive(Debug)]
pub struct ExeSymOps {
    /// 存储 PID，在读取时动态查找进程
    pid: RawPid,
}

impl ExeSymOps {
    pub fn new(pid: RawPid) -> Self {
        Self { pid }
    }

    pub fn new_inode(pid: RawPid, parent: Weak<dyn IndexNode>) -> Arc<dyn IndexNode> {
        ProcSymBuilder::new(Self::new(pid), ModeType::S_IRWXUGO) // 0777 - 符号链接权限
            .parent(parent)
            .build()
            .unwrap()
    }
}

impl SymOps for ExeSymOps {
    fn read_link(&self, buf: &mut [u8]) -> Result<usize, SystemError> {
        // 动态查找进程，获取目标进程的可执行文件路径
        let pcb = ProcessManager::find(self.pid).ok_or(SystemError::ESRCH)?;
        let exe = pcb.execute_path();
        let exe_bytes = exe.as_bytes();
        let len = exe_bytes.len().min(buf.len());
        buf[..len].copy_from_slice(&exe_bytes[..len]);
        Ok(len)
    }
}
