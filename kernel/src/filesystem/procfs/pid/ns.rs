//! /proc/[pid]/ns - 进程命名空间目录
//!
//! 提供进程的命名空间符号链接，每个链接指向对应的命名空间标识符

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
        ProcessManager, RawPid,
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

/// 获取指定进程的命名空间 ID
fn get_pid_ns_ino(pid: RawPid, ns_type: NsFileType) -> Result<usize, SystemError> {
    let pcb = ProcessManager::find(pid).ok_or(SystemError::ESRCH)?;
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

    Ok(ino.data())
}

/// /proc/[pid]/ns 目录的 DirOps 实现
#[derive(Debug)]
pub struct NsDirOps {
    pid: RawPid,
}

impl NsDirOps {
    pub fn new_inode(pid: RawPid, parent: Weak<dyn IndexNode>) -> Arc<dyn IndexNode> {
        ProcDirBuilder::new(Self { pid }, InodeMode::from_bits_truncate(0o555))
            .parent(parent)
            .volatile()
            .build()
            .unwrap()
    }
}

impl DirOps for NsDirOps {
    fn lookup_child(
        &self,
        dir: &ProcDir<Self>,
        name: &str,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        // 解析命名空间类型
        let ns_type = NsFileType::try_from(name)?;

        // 检查进程是否存在
        if ProcessManager::find(self.pid).is_none() {
            return Err(SystemError::ESRCH);
        }

        let mut cached_children = dir.cached_children().write();
        if let Some(child) = cached_children.get(name) {
            return Ok(child.clone());
        }

        // 创建命名空间符号链接
        let inode = NsSymOps::new_inode(self.pid, ns_type, dir.self_ref_weak().clone());
        cached_children.insert(name.to_string(), inode.clone());
        Ok(inode)
    }

    fn populate_children(&self, dir: &ProcDir<Self>) {
        // 检查进程是否存在
        if ProcessManager::find(self.pid).is_none() {
            return;
        }

        let mut cached_children = dir.cached_children().write();

        for name in NsFileType::ALL_NAMES {
            if let Ok(ns_type) = NsFileType::try_from(name) {
                cached_children.entry(name.to_string()).or_insert_with(|| {
                    NsSymOps::new_inode(self.pid, ns_type, dir.self_ref_weak().clone())
                });
            }
        }
    }
}

/// /proc/[pid]/ns/[type] 符号链接的 SymOps 实现
#[derive(Debug)]
pub struct NsSymOps {
    pid: RawPid,
    ns_type: NsFileType,
}

impl NsSymOps {
    pub fn new_inode(
        pid: RawPid,
        ns_type: NsFileType,
        parent: Weak<dyn IndexNode>,
    ) -> Arc<dyn IndexNode> {
        ProcSymBuilder::new(Self { pid, ns_type }, InodeMode::S_IRWXUGO)
            .parent(parent)
            .build()
            .unwrap()
    }
}

impl SymOps for NsSymOps {
    fn read_link(&self, buf: &mut [u8]) -> Result<usize, SystemError> {
        let ino = get_pid_ns_ino(self.pid, self.ns_type)?;
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
        get_pid_ns_ino(self.pid, self.ns_type)
            .ok()
            .map(InodeId::new)
    }

    fn open(&self, data: &mut SpinLockGuard<FilePrivateData>) -> Result<(), SystemError> {
        // 当打开命名空间文件时，设置命名空间私有数据
        // 这使得 setns() 可以使用这个 fd
        let pcb = ProcessManager::find(self.pid).ok_or(SystemError::ESRCH)?;
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
