//! /proc/thread-self - 指向当前线程目录的符号链接
//!
//! Linux 中 /proc/thread-self 是一个魔法符号链接，目标为
//! /proc/<tgid>/task/<tid>，其中数字必须按当前 proc mount 绑定的 pid namespace
//! 生成。真正的命名空间文件语义则由 /proc/<tgid>/task/<tid>/ns/* 提供。

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
    format,
    sync::{Arc, Weak},
};
use core::{convert::TryFrom, fmt};
use system_error::SystemError;

/// 命名空间文件类型
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NsFileType {
    Ipc,
    Uts,
    Mnt,
    Net,
    Pid,
    PidForChildren,
    Time,
    TimeForChildren,
    User,
    Cgroup,
}

impl NsFileType {
    /// 所有命名空间类型名称
    pub const ALL_NAMES: [&'static str; 10] = [
        "ipc",
        "uts",
        "mnt",
        "net",
        "pid",
        "pid_for_children",
        "time",
        "time_for_children",
        "user",
        "cgroup",
    ];

    /// 获取命名空间类型名称
    pub const fn name(&self) -> &'static str {
        match self {
            NsFileType::Ipc => "ipc",
            NsFileType::Uts => "uts",
            NsFileType::Mnt => "mnt",
            NsFileType::Net => "net",
            NsFileType::Pid => "pid",
            NsFileType::PidForChildren => "pid_for_children",
            NsFileType::Time => "time",
            NsFileType::TimeForChildren => "time_for_children",
            NsFileType::User => "user",
            NsFileType::Cgroup => "cgroup",
        }
    }
}

impl TryFrom<&str> for NsFileType {
    type Error = SystemError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        match value {
            "ipc" => Ok(NsFileType::Ipc),
            "uts" => Ok(NsFileType::Uts),
            "mnt" => Ok(NsFileType::Mnt),
            "net" => Ok(NsFileType::Net),
            "pid" => Ok(NsFileType::Pid),
            "pid_for_children" => Ok(NsFileType::PidForChildren),
            "time" => Ok(NsFileType::Time),
            "time_for_children" => Ok(NsFileType::TimeForChildren),
            "user" => Ok(NsFileType::User),
            "cgroup" => Ok(NsFileType::Cgroup),
            _ => Err(SystemError::ENOENT),
        }
    }
}

/// /proc/thread-self 符号链接的 SymOps 实现
pub struct ThreadSelfSymOps {
    view_pid_ns: Arc<PidNamespace>,
}

impl fmt::Debug for ThreadSelfSymOps {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ThreadSelfSymOps").finish()
    }
}

impl ThreadSelfSymOps {
    pub fn new_inode(parent: Weak<dyn IndexNode>) -> Arc<dyn IndexNode> {
        let view_pid_ns = parent
            .upgrade()
            .expect("proc thread-self parent should exist")
            .fs()
            .as_any_ref()
            .downcast_ref::<ProcFS>()
            .expect("/proc/thread-self must belong to procfs")
            .pid_ns()
            .clone();

        ProcSymBuilder::new(Self { view_pid_ns }, InodeMode::S_IRWXUGO)
            .parent(parent)
            .build()
            .unwrap()
    }
}

impl SymOps for ThreadSelfSymOps {
    fn read_link(&self, buf: &mut [u8]) -> Result<usize, SystemError> {
        let current_pcb = ProcessManager::current_pcb();
        let tgid = current_pcb
            .task_pid_ptr(PidType::TGID)
            .map(|pid| pid.pid_nr_ns(&self.view_pid_ns))
            .ok_or(SystemError::ESRCH)?;
        let tid = current_pcb
            .task_pid_ptr(PidType::PID)
            .map(|pid| pid.pid_nr_ns(&self.view_pid_ns))
            .ok_or(SystemError::ESRCH)?;

        if tgid.data() == 0 || tid.data() == 0 {
            return Err(SystemError::ENOENT);
        }

        let target = format!("{}/task/{}", tgid.data(), tid.data());
        let len = target.len().min(buf.len());
        buf[..len].copy_from_slice(&target.as_bytes()[..len]);
        Ok(len)
    }
}
