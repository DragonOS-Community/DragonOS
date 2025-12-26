//! /proc/thread-self 目录
//!
//! 这个模块实现了 /proc/thread-self 目录，它包含当前线程的信息。
//! 主要提供 /proc/thread-self/ns/ 子目录用于访问当前线程的命名空间。

use crate::{
    filesystem::{
        procfs::template::{Builder, DirOps, ProcDir, ProcDirBuilder, ProcSymBuilder, SymOps},
        vfs::{
            file::{FilePrivateData, NamespaceFilePrivateData},
            IndexNode, InodeId, InodeMode,
        },
    },
    libs::spinlock::SpinLockGuard,
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

/// 获取当前线程的命名空间 ID
fn current_thread_self_ns_ino(ns_type: NsFileType) -> usize {
    let pcb = ProcessManager::current_pcb();
    let nsproxy = pcb.nsproxy();

    let ino: NamespaceId = match ns_type {
        NsFileType::Ipc => nsproxy.ipc_ns.ns_common().nsid,
        NsFileType::Uts => nsproxy.uts_ns.ns_common().nsid,
        NsFileType::Mnt => nsproxy.mnt_ns.ns_common().nsid,
        NsFileType::Net => nsproxy.net_ns.ns_common().nsid,
        NsFileType::Pid => pcb.active_pid_ns().ns_common().nsid,
        NsFileType::PidForChildren => nsproxy.pid_ns_for_children.ns_common().nsid,
        NsFileType::Time | NsFileType::TimeForChildren => {
            // Time namespace 尚未实现
            NamespaceId::new(0)
        }
        NsFileType::User => pcb.cred().user_ns.ns_common().nsid,
        NsFileType::Cgroup => nsproxy.cgroup_ns.ns_common().nsid,
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
        let ns_type = NsFileType::try_from(name)?;

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

        for name in NsFileType::ALL_NAMES {
            if let Ok(ns_type) = NsFileType::try_from(name) {
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
    ns_type: NsFileType,
}

impl ThreadSelfNsSymOps {
    pub fn new_inode(ns_type: NsFileType, parent: Weak<dyn IndexNode>) -> Arc<dyn IndexNode> {
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

    fn is_self_reference(&self) -> bool {
        // 命名空间符号链接是自引用的魔法链接
        true
    }

    fn dynamic_inode_id(&self) -> Option<InodeId> {
        // 命名空间文件的 inode ID 应该是命名空间的 ID
        // 这样 stat() 返回的 st_ino 就是命名空间 ID
        Some(InodeId::new(current_thread_self_ns_ino(self.ns_type)))
    }

    fn open(&self, data: &mut SpinLockGuard<FilePrivateData>) -> Result<(), SystemError> {
        // 当打开命名空间文件时，设置命名空间私有数据
        // 这使得 setns() 可以使用这个 fd
        let pcb = ProcessManager::current_pcb();
        let nsproxy = pcb.nsproxy();

        let ns_data = match self.ns_type {
            NsFileType::Ipc => NamespaceFilePrivateData::Ipc(nsproxy.ipc_ns.clone()),
            NsFileType::Uts => NamespaceFilePrivateData::Uts(nsproxy.uts_ns.clone()),
            NsFileType::Mnt => NamespaceFilePrivateData::Mnt(nsproxy.mnt_ns.clone()),
            NsFileType::Net => NamespaceFilePrivateData::Net(nsproxy.net_ns.clone()),
            NsFileType::Pid => NamespaceFilePrivateData::Pid(pcb.active_pid_ns()),
            NsFileType::PidForChildren => {
                NamespaceFilePrivateData::PidForChildren(nsproxy.pid_ns_for_children.clone())
            }
            NsFileType::Time | NsFileType::TimeForChildren => {
                // Time namespace 尚未实现
                return Err(SystemError::ENOSYS);
            }
            NsFileType::User => NamespaceFilePrivateData::User(pcb.cred().user_ns.clone()),
            NsFileType::Cgroup => NamespaceFilePrivateData::Cgroup(nsproxy.cgroup_ns.clone()),
        };

        **data = FilePrivateData::Namespace(ns_data);
        Ok(())
    }
}
