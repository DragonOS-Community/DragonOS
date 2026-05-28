//! /proc/self - 指向当前进程目录的符号链接
//!
//! /proc/self 是一个指向 /proc/[pid] 的符号链接，其中 pid 是当前进程的 PID

use crate::{
    filesystem::{
        procfs::{
            root::ProcFS,
            template::{Builder, ProcSymBuilder, SymOps},
        },
        vfs::{IndexNode, InodeMode},
    },
    process::{namespace::pid_namespace::PidNamespace, pid::PidType, ProcessManager},
};
use alloc::{
    string::ToString,
    sync::{Arc, Weak},
};
use core::fmt;
use system_error::SystemError;

/// /proc/self 符号链接的 SymOps 实现
pub struct SelfSymOps {
    view_pid_ns: Arc<PidNamespace>,
}

impl fmt::Debug for SelfSymOps {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SelfSymOps").finish()
    }
}

impl SelfSymOps {
    pub fn new_inode(parent: Weak<dyn IndexNode>) -> Arc<dyn IndexNode> {
        let view_pid_ns = parent
            .upgrade()
            .expect("proc self parent should exist")
            .fs()
            .as_any_ref()
            .downcast_ref::<ProcFS>()
            .expect("/proc/self must belong to procfs")
            .pid_ns()
            .clone();

        ProcSymBuilder::new(Self { view_pid_ns }, InodeMode::S_IRWXUGO) // 0777 - 符号链接权限
            .parent(parent)
            .build()
            .unwrap()
    }
}

impl SymOps for SelfSymOps {
    fn read_link(&self, buf: &mut [u8]) -> Result<usize, SystemError> {
        let current_pcb = ProcessManager::current_pcb();
        let tgid = current_pcb
            .task_pid_ptr(PidType::TGID)
            .map(|pid| pid.pid_nr_ns(&self.view_pid_ns))
            .ok_or(SystemError::ESRCH)?;
        if tgid.data() == 0 {
            return Err(SystemError::ENOENT);
        }
        let pid_bytes = tgid.data().to_string();
        let len = pid_bytes.len().min(buf.len());
        buf[..len].copy_from_slice(&pid_bytes.as_bytes()[..len]);
        Ok(len)
    }
}
