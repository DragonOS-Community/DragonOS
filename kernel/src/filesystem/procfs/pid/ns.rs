//! /proc/[pid]/ns - 进程命名空间目录
//!
//! 提供进程的命名空间符号链接，每个链接指向对应的命名空间标识符

use crate::libs::mutex::MutexGuard;
use crate::{
    filesystem::{
        procfs::{
            pid::ProcPidTarget,
            template::{
                Builder, DirOps, FileOps, ProcDir, ProcDirBuilder, ProcFileBuilder, ProcSymBuilder,
                SymOps,
            },
            thread_self::NsFileType,
        },
        vfs::{
            file::{FilePrivateData, NamespaceFilePrivateData},
            utils::DName,
            FileSystem, IndexNode, InodeId, InodeMode, SpecialNodeData,
        },
    },
    process::namespace::{nsproxy::NamespaceId, NamespaceOps},
};
use alloc::{
    format,
    string::ToString,
    sync::{Arc, Weak},
};
use core::convert::TryFrom;
use system_error::SystemError;

#[derive(Debug)]
struct NamespaceSnapshot {
    data: NamespaceFilePrivateData,
    nsid: NamespaceId,
}

fn namespace_snapshot(
    target: &ProcPidTarget,
    ns_type: NsFileType,
) -> Result<NamespaceSnapshot, SystemError> {
    let pcb = target.task().ok_or(SystemError::ESRCH)?;
    let nsproxy = pcb.nsproxy();

    let data = match ns_type {
        NsFileType::Ipc => NamespaceFilePrivateData::Ipc(nsproxy.ipc_ns.clone()),
        NsFileType::Uts => NamespaceFilePrivateData::Uts(nsproxy.uts_ns.clone()),
        NsFileType::Mnt => NamespaceFilePrivateData::Mnt(nsproxy.mnt_ns.clone()),
        NsFileType::Net => NamespaceFilePrivateData::Net(nsproxy.net_ns.clone()),
        NsFileType::Pid => {
            NamespaceFilePrivateData::Pid(pcb.try_active_pid_ns().ok_or(SystemError::ESRCH)?)
        }
        NsFileType::PidForChildren => {
            NamespaceFilePrivateData::PidForChildren(nsproxy.pid_ns_for_children.clone())
        }
        NsFileType::Time | NsFileType::TimeForChildren => {
            return Err(SystemError::ENOSYS);
        }
        NsFileType::User => NamespaceFilePrivateData::User(pcb.cred().user_ns.clone()),
        NsFileType::Cgroup => NamespaceFilePrivateData::Cgroup(nsproxy.cgroup_ns.clone()),
    };

    let nsid = match &data {
        NamespaceFilePrivateData::Ipc(ns) => ns.ns_common().nsid,
        NamespaceFilePrivateData::Uts(ns) => ns.ns_common().nsid,
        NamespaceFilePrivateData::Mnt(ns) => ns.ns_common().nsid,
        NamespaceFilePrivateData::Net(ns) => ns.ns_common().nsid,
        NamespaceFilePrivateData::Pid(ns) | NamespaceFilePrivateData::PidForChildren(ns) => {
            ns.ns_common().nsid
        }
        NamespaceFilePrivateData::User(ns) => ns.ns_common().nsid,
        NamespaceFilePrivateData::Cgroup(ns) => ns.ns_common().nsid,
    };

    Ok(NamespaceSnapshot { data, nsid })
}

/// /proc/[pid]/ns 目录的 DirOps 实现
#[derive(Debug)]
pub struct NsDirOps {
    target: ProcPidTarget,
}

impl NsDirOps {
    pub fn new_inode(target: ProcPidTarget, parent: Weak<dyn IndexNode>) -> Arc<dyn IndexNode> {
        ProcDirBuilder::new(Self { target }, InodeMode::from_bits_truncate(0o555))
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
        if self.target.task().is_none() {
            return Err(SystemError::ESRCH);
        }

        let mut cached_children = dir.cached_children().write();
        if let Some(child) = cached_children.get(name) {
            return Ok(child.clone());
        }

        // 创建命名空间符号链接
        let inode = NsSymOps::new_inode(
            self.target.clone(),
            ns_type,
            Arc::downgrade(&dir.fs()),
            dir.self_ref_weak().clone(),
        );
        cached_children.insert(name.to_string(), inode.clone());
        Ok(inode)
    }

    fn populate_children(&self, dir: &ProcDir<Self>) {
        if self.target.task().is_none() {
            return;
        }

        let mut cached_children = dir.cached_children().write();

        for name in NsFileType::ALL_NAMES {
            if let Ok(ns_type) = NsFileType::try_from(name) {
                cached_children.entry(name.to_string()).or_insert_with(|| {
                    NsSymOps::new_inode(
                        self.target.clone(),
                        ns_type,
                        Arc::downgrade(&dir.fs()),
                        dir.self_ref_weak().clone(),
                    )
                });
            }
        }
    }
}

/// /proc/[pid]/ns/[type] 符号链接的 SymOps 实现
#[derive(Debug)]
pub struct NsSymOps {
    target: ProcPidTarget,
    ns_type: NsFileType,
    fs: Weak<dyn FileSystem>,
}

impl NsSymOps {
    pub fn new_inode(
        target: ProcPidTarget,
        ns_type: NsFileType,
        fs: Weak<dyn FileSystem>,
        parent: Weak<dyn IndexNode>,
    ) -> Arc<dyn IndexNode> {
        ProcSymBuilder::new(
            Self {
                target,
                ns_type,
                fs,
            },
            InodeMode::S_IRWXUGO,
        )
        .parent(parent)
        .build()
        .unwrap()
    }
}

#[derive(Debug)]
struct NamespaceFileOps {
    snapshot: NamespaceSnapshot,
}

impl FileOps for NamespaceFileOps {
    fn read_at(
        &self,
        _offset: usize,
        _len: usize,
        _buf: &mut [u8],
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        Err(SystemError::EINVAL)
    }

    fn open(&self, data: &mut MutexGuard<FilePrivateData>) -> Result<(), SystemError> {
        **data = FilePrivateData::Namespace(self.snapshot.data.clone());
        Ok(())
    }

    fn dynamic_inode_id(&self) -> Option<InodeId> {
        Some(InodeId::new(self.snapshot.nsid.data()))
    }
}

impl SymOps for NsSymOps {
    fn read_link(&self, buf: &mut [u8]) -> Result<usize, SystemError> {
        let ino = namespace_snapshot(&self.target, self.ns_type)?.nsid.data();
        let target = format!("{}:[{}]", self.ns_type.name(), ino);
        let len = target.len().min(buf.len());
        buf[..len].copy_from_slice(&target.as_bytes()[..len]);
        Ok(len)
    }

    fn special_node(&self) -> Option<SpecialNodeData> {
        let snapshot = namespace_snapshot(&self.target, self.ns_type).ok()?;
        let dname = DName::from(format!(
            "{}:[{}]",
            self.ns_type.name(),
            snapshot.nsid.data()
        ));
        let inode = ProcFileBuilder::new(NamespaceFileOps { snapshot }, InodeMode::S_IRUGO)
            .fs(self.fs.clone())
            .build()
            .ok()?;
        Some(SpecialNodeData::MountProjectedReference {
            target: inode,
            dname,
        })
    }

    fn dynamic_inode_id(&self) -> Option<InodeId> {
        namespace_snapshot(&self.target, self.ns_type)
            .ok()
            .map(|snapshot| InodeId::new(snapshot.nsid.data()))
    }

    fn open(&self, data: &mut MutexGuard<FilePrivateData>) -> Result<(), SystemError> {
        **data = FilePrivateData::Namespace(namespace_snapshot(&self.target, self.ns_type)?.data);
        Ok(())
    }
}
