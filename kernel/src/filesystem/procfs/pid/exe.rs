//! /proc/[pid]/exe - 进程可执行文件的符号链接
//!
//! 这个符号链接指向进程的可执行文件路径

use crate::filesystem::{
    procfs::{
        pid::ProcPidTarget,
        template::{Builder, ProcSymBuilder, SymOps},
    },
    vfs::{IndexNode, InodeMode},
};
use alloc::sync::{Arc, Weak};
use system_error::SystemError;

/// /proc/[pid]/exe 符号链接的 SymOps 实现
#[derive(Debug)]
pub struct ExeSymOps {
    target: ProcPidTarget,
}

impl ExeSymOps {
    pub fn new_inode(target: ProcPidTarget, parent: Weak<dyn IndexNode>) -> Arc<dyn IndexNode> {
        ProcSymBuilder::new(Self { target }, InodeMode::S_IRWXUGO) // 0777 - 符号链接权限
            .parent(parent)
            .build()
            .unwrap()
    }
}

impl SymOps for ExeSymOps {
    fn read_link(&self, buf: &mut [u8]) -> Result<usize, SystemError> {
        let pcb = self.target.task().ok_or(SystemError::ESRCH)?;
        let exe = pcb.execute_path();
        let exe_bytes = exe.as_bytes();
        let len = exe_bytes.len().min(buf.len());
        buf[..len].copy_from_slice(&exe_bytes[..len]);
        Ok(len)
    }
}
