//! /proc/[pid]/task - 进程线程目录
//!
//! 列出进程的所有线程，每个线程对应一个子目录 /proc/[pid]/task/[tid]

use crate::{
    filesystem::{
        procfs::{
            pid::stat::StatFileOps,
            template::{
                lookup_child_from_table, populate_children_from_table, DirOps, ProcDir,
                ProcDirBuilder,
            },
            Builder,
        },
        vfs::{IndexNode, InodeMode},
    },
    process::{ProcessManager, RawPid},
};
use alloc::{
    string::ToString,
    sync::{Arc, Weak},
};
use system_error::SystemError;

/// /proc/[pid]/task 目录的 DirOps 实现
#[derive(Debug)]
pub struct TaskDirOps {
    pid: RawPid,
}

impl TaskDirOps {
    pub fn new_inode(pid: RawPid, parent: Weak<dyn IndexNode>) -> Arc<dyn IndexNode> {
        ProcDirBuilder::new(Self { pid }, InodeMode::from_bits_truncate(0o555))
            .parent(parent)
            .volatile()
            .build()
            .unwrap()
    }
}

impl DirOps for TaskDirOps {
    fn lookup_child(
        &self,
        dir: &ProcDir<Self>,
        name: &str,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        // 解析 tid
        let tid = name.parse::<usize>().map_err(|_| SystemError::ENOENT)?;
        let tid_pid = RawPid::new(tid);

        // 目前简化实现：只支持 tid == pid（主线程）
        // TODO: 支持真正的多线程
        if tid_pid != self.pid {
            return Err(SystemError::ENOENT);
        }

        // 检查进程是否存在
        if ProcessManager::find(self.pid).is_none() {
            return Err(SystemError::ESRCH);
        }

        let mut cached_children = dir.cached_children().write();
        if let Some(child) = cached_children.get(name) {
            return Ok(child.clone());
        }

        // 创建 tid 目录
        let inode = TidDirOps::new_inode(self.pid, tid_pid, dir.self_ref_weak().clone());
        cached_children.insert(name.to_string(), inode.clone());
        Ok(inode)
    }

    fn populate_children(&self, dir: &ProcDir<Self>) {
        // 检查进程是否存在
        if ProcessManager::find(self.pid).is_none() {
            return;
        }

        let mut cached_children = dir.cached_children().write();
        let tid_str = self.pid.to_string();

        // 目前只添加主线程
        cached_children.entry(tid_str).or_insert_with(|| {
            TidDirOps::new_inode(self.pid, self.pid, dir.self_ref_weak().clone())
        });
    }
}

/// /proc/[pid]/task/[tid] 目录的 DirOps 实现
#[derive(Debug)]
pub struct TidDirOps {
    pid: RawPid,
    #[allow(dead_code)]
    tid: RawPid,
}

impl TidDirOps {
    pub fn new_inode(pid: RawPid, tid: RawPid, parent: Weak<dyn IndexNode>) -> Arc<dyn IndexNode> {
        ProcDirBuilder::new(Self { pid, tid }, InodeMode::from_bits_truncate(0o555))
            .parent(parent)
            .volatile()
            .build()
            .unwrap()
    }

    /// 静态条目表
    #[expect(clippy::type_complexity)]
    const STATIC_ENTRIES: &'static [(
        &'static str,
        fn(&TidDirOps, Weak<dyn IndexNode>) -> Arc<dyn IndexNode>,
    )] = &[("stat", |ops, parent| {
        StatFileOps::new_inode(ops.pid, parent)
    })];
}

impl DirOps for TidDirOps {
    fn lookup_child(
        &self,
        dir: &ProcDir<Self>,
        name: &str,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        let mut cached_children = dir.cached_children().write();

        if let Some(child) =
            lookup_child_from_table(name, &mut cached_children, Self::STATIC_ENTRIES, |f| {
                (f)(self, dir.self_ref_weak().clone())
            })
        {
            return Ok(child);
        }

        Err(SystemError::ENOENT)
    }

    fn populate_children(&self, dir: &ProcDir<Self>) {
        let mut cached_children = dir.cached_children().write();
        populate_children_from_table(&mut cached_children, Self::STATIC_ENTRIES, |f| {
            (f)(self, dir.self_ref_weak().clone())
        });
    }
}
