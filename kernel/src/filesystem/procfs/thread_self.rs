//! /proc/thread-self 目录
//!
//! 这个模块实现了 /proc/thread-self 目录，它包含当前线程的信息。
//! 主要提供 /proc/thread-self/ns/ 子目录用于访问当前线程的命名空间。

use crate::{
    filesystem::{
        procfs::template::{Builder, DirOps, ProcDir, ProcDirBuilder, ProcSymBuilder, SymOps},
        vfs::{IndexNode, InodeMode},
    },
    process::{
        namespace::{nsproxy::NamespaceId, NamespaceOps},
        ProcessManager,
    },
};
use alloc::{
    format,
    string::ToString,
    sync::{Arc, Weak},
};
use core::convert::TryFrom;
use system_error::SystemError;

/// 命名空间文件类型
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThreadSelfNsFileType {
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

impl ThreadSelfNsFileType {
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
            ThreadSelfNsFileType::Ipc => "ipc",
            ThreadSelfNsFileType::Uts => "uts",
            ThreadSelfNsFileType::Mnt => "mnt",
            ThreadSelfNsFileType::Net => "net",
            ThreadSelfNsFileType::Pid => "pid",
            ThreadSelfNsFileType::PidForChildren => "pid_for_children",
            ThreadSelfNsFileType::Time => "time",
            ThreadSelfNsFileType::TimeForChildren => "time_for_children",
            ThreadSelfNsFileType::User => "user",
            ThreadSelfNsFileType::Cgroup => "cgroup",
        }
    }
}

impl TryFrom<&str> for ThreadSelfNsFileType {
    type Error = SystemError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        match value {
            "ipc" => Ok(ThreadSelfNsFileType::Ipc),
            "uts" => Ok(ThreadSelfNsFileType::Uts),
            "mnt" => Ok(ThreadSelfNsFileType::Mnt),
            "net" => Ok(ThreadSelfNsFileType::Net),
            "pid" => Ok(ThreadSelfNsFileType::Pid),
            "pid_for_children" => Ok(ThreadSelfNsFileType::PidForChildren),
            "time" => Ok(ThreadSelfNsFileType::Time),
            "time_for_children" => Ok(ThreadSelfNsFileType::TimeForChildren),
            "user" => Ok(ThreadSelfNsFileType::User),
            "cgroup" => Ok(ThreadSelfNsFileType::Cgroup),
            _ => Err(SystemError::ENOENT),
        }
    }
}

/// 获取当前线程的命名空间 ID
fn current_thread_self_ns_ino(ns_type: ThreadSelfNsFileType) -> usize {
    let pcb = ProcessManager::current_pcb();
    let nsproxy = pcb.nsproxy();

    let ino: NamespaceId = match ns_type {
        ThreadSelfNsFileType::Ipc => nsproxy.ipc_ns.ns_common().nsid,
        ThreadSelfNsFileType::Uts => nsproxy.uts_ns.ns_common().nsid,
        ThreadSelfNsFileType::Mnt => nsproxy.mnt_ns.ns_common().nsid,
        ThreadSelfNsFileType::Net => nsproxy.net_ns.ns_common().nsid,
        ThreadSelfNsFileType::Pid => pcb.active_pid_ns().ns_common().nsid,
        ThreadSelfNsFileType::PidForChildren => nsproxy.pid_ns_for_children.ns_common().nsid,
        ThreadSelfNsFileType::Time | ThreadSelfNsFileType::TimeForChildren => {
            // Time namespace 尚未实现
            NamespaceId::new(0)
        }
        ThreadSelfNsFileType::User => pcb.cred().user_ns.ns_common().nsid,
        ThreadSelfNsFileType::Cgroup => nsproxy.cgroup_ns.ns_common().nsid,
    };

    ino.data()
}

// ============================================================================
// /proc/thread-self 目录
// ============================================================================

/// /proc/thread-self 目录的 DirOps 实现
#[derive(Debug)]
pub struct ThreadSelfDirOps;

impl ThreadSelfDirOps {
    pub fn new_inode(parent: Weak<dyn IndexNode>) -> Arc<dyn IndexNode> {
        ProcDirBuilder::new(Self, InodeMode::from_bits_truncate(0o555))
            .parent(parent)
            .build()
            .unwrap()
    }
}

impl DirOps for ThreadSelfDirOps {
    fn lookup_child(
        &self,
        dir: &ProcDir<Self>,
        name: &str,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        if name == "ns" {
            let mut cached_children = dir.cached_children().write();
            if let Some(child) = cached_children.get(name) {
                return Ok(child.clone());
            }

            let inode = ThreadSelfNsDirOps::new_inode(dir.self_ref_weak().clone());
            cached_children.insert(name.to_string(), inode.clone());
            return Ok(inode);
        }

        Err(SystemError::ENOENT)
    }

    fn populate_children(&self, dir: &ProcDir<Self>) {
        let mut cached_children = dir.cached_children().write();
        cached_children
            .entry("ns".to_string())
            .or_insert_with(|| ThreadSelfNsDirOps::new_inode(dir.self_ref_weak().clone()));
    }
}

// ============================================================================
// /proc/thread-self/ns 目录
// ============================================================================

/// /proc/thread-self/ns 目录的 DirOps 实现
#[derive(Debug)]
pub struct ThreadSelfNsDirOps;

impl ThreadSelfNsDirOps {
    pub fn new_inode(parent: Weak<dyn IndexNode>) -> Arc<dyn IndexNode> {
        ProcDirBuilder::new(Self, InodeMode::from_bits_truncate(0o555))
            .parent(parent)
            .build()
            .unwrap()
    }
}

impl DirOps for ThreadSelfNsDirOps {
    fn lookup_child(
        &self,
        dir: &ProcDir<Self>,
        name: &str,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        // 解析命名空间类型
        let ns_type = ThreadSelfNsFileType::try_from(name)?;

        let mut cached_children = dir.cached_children().write();
        if let Some(child) = cached_children.get(name) {
            return Ok(child.clone());
        }

        // 创建命名空间符号链接
        let inode = ThreadSelfNsSymOps::new_inode(ns_type, dir.self_ref_weak().clone());
        cached_children.insert(name.to_string(), inode.clone());
        Ok(inode)
    }

    fn populate_children(&self, dir: &ProcDir<Self>) {
        let mut cached_children = dir.cached_children().write();

        for name in ThreadSelfNsFileType::ALL_NAMES {
            if let Ok(ns_type) = ThreadSelfNsFileType::try_from(name) {
                cached_children.entry(name.to_string()).or_insert_with(|| {
                    ThreadSelfNsSymOps::new_inode(ns_type, dir.self_ref_weak().clone())
                });
            }
        }
    }
}

// ============================================================================
// /proc/thread-self/ns/* 符号链接
// ============================================================================

/// /proc/thread-self/ns/[type] 符号链接的 SymOps 实现
#[derive(Debug)]
pub struct ThreadSelfNsSymOps {
    ns_type: ThreadSelfNsFileType,
}

impl ThreadSelfNsSymOps {
    pub fn new_inode(
        ns_type: ThreadSelfNsFileType,
        parent: Weak<dyn IndexNode>,
    ) -> Arc<dyn IndexNode> {
        ProcSymBuilder::new(Self { ns_type }, InodeMode::S_IRWXUGO)
            .parent(parent)
            .build()
            .unwrap()
    }
}

impl SymOps for ThreadSelfNsSymOps {
    fn read_link(&self, buf: &mut [u8]) -> Result<usize, SystemError> {
        let ino = current_thread_self_ns_ino(self.ns_type);
        let target = format!("{}:[{}]", self.ns_type.name(), ino);
        let len = target.len().min(buf.len());
        buf[..len].copy_from_slice(&target.as_bytes()[..len]);
        Ok(len)
    }
}
