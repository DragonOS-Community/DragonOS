//! /proc/self - 指向当前进程目录的符号链接
//!
//! /proc/self 是一个指向 /proc/[pid] 的符号链接，其中 pid 是当前进程的 PID

use crate::{
    filesystem::{
        procfs::template::{Builder, ProcSymBuilder, SymOps},
        vfs::{IndexNode, InodeMode},
    },
    process::ProcessManager,
};
use alloc::{
    string::ToString,
    sync::{Arc, Weak},
};
use system_error::SystemError;

/// /proc/self 符号链接的 SymOps 实现
#[derive(Debug)]
pub struct SelfSymOps;

impl SelfSymOps {
    pub fn new_inode(parent: Weak<dyn IndexNode>) -> Arc<dyn IndexNode> {
        ProcSymBuilder::new(Self, InodeMode::S_IRWXUGO) // 0777 - 符号链接权限
            .parent(parent)
            .build()
            .unwrap()
    }
}

impl SymOps for SelfSymOps {
    fn read_link(&self, buf: &mut [u8]) -> Result<usize, SystemError> {
        // 返回当前线程组的 TGID（与 Linux /proc/self 语义一致）
        let current_pcb = ProcessManager::current_pcb();
        let tgid = current_pcb
            .task_tgid_vnr()
            .unwrap_or_else(ProcessManager::current_pid);
        let pid_bytes = tgid.data().to_string();
        let len = pid_bytes.len().min(buf.len());
        buf[..len].copy_from_slice(&pid_bytes.as_bytes()[..len]);
        Ok(len)
    }
}
