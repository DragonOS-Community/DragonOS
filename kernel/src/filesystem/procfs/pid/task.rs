//! /proc/[pid]/task - 进程线程目录
//!
//! 列出进程的所有线程，每个线程对应一个子目录 /proc/[pid]/task/[tid]

use super::oom_score_adj::OomScoreAdjFileOps;

use crate::{
    filesystem::{
        procfs::{
            pid::{ns::NsDirOps, stat::StatFileOps, ProcPidTarget},
            template::{
                lookup_child_from_table, populate_children_from_table, DirOps, ProcDir,
                ProcDirBuilder,
            },
            Builder,
        },
        vfs::{IndexNode, InodeMode},
    },
    process::ProcessControlBlock,
};
use alloc::{
    string::ToString,
    sync::{Arc, Weak},
    vec::Vec,
};
use system_error::SystemError;

/// /proc/[pid]/task 目录的 DirOps 实现
#[derive(Debug)]
pub struct TaskDirOps {
    target: ProcPidTarget,
}

impl TaskDirOps {
    pub fn new_inode(target: ProcPidTarget, parent: Weak<dyn IndexNode>) -> Arc<dyn IndexNode> {
        ProcDirBuilder::new(Self { target }, InodeMode::from_bits_truncate(0o555))
            .parent(parent)
            .volatile()
            .build()
            .unwrap()
    }

    fn thread_group_leader(&self) -> Option<Arc<ProcessControlBlock>> {
        self.target.thread_group_leader()
    }

    fn thread_targets(&self) -> Vec<ProcPidTarget> {
        let Some(leader) = self.thread_group_leader() else {
            return Vec::new();
        };

        let mut targets = Vec::new();
        if let Some(target) =
            ProcPidTarget::from_task(self.target.view_pid_ns().clone(), leader.clone())
        {
            targets.push(target);
        }

        let group_tasks = leader.threads_read_irqsave().group_tasks_clone();
        for weak in group_tasks {
            if let Some(task) = weak.upgrade() {
                if let Some(target) =
                    ProcPidTarget::from_task(self.target.view_pid_ns().clone(), task)
                {
                    targets.push(target);
                }
            }
        }

        targets.sort_by_key(|target| target.vpid().data());
        targets.dedup_by_key(|target| target.vpid().data());
        targets
    }
}

impl DirOps for TaskDirOps {
    fn lookup_child(
        &self,
        dir: &ProcDir<Self>,
        name: &str,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        // 解析 tid
        if self.thread_group_leader().is_none() {
            return Err(SystemError::ESRCH);
        }

        let tid = name.parse::<usize>().map_err(|_| SystemError::ENOENT)?;
        let target = self
            .thread_targets()
            .into_iter()
            .find(|target| target.vpid().data() == tid)
            .ok_or(SystemError::ENOENT)?;

        let mut cached_children = dir.cached_children().write();
        if let Some(child) = cached_children.get(name) {
            return Ok(child.clone());
        }

        let inode = TidDirOps::new_inode(target, dir.self_ref_weak().clone());
        cached_children.insert(name.to_string(), inode.clone());
        Ok(inode)
    }

    fn populate_children(&self, dir: &ProcDir<Self>) {
        if self.thread_group_leader().is_none() {
            return;
        }

        let mut cached_children = dir.cached_children().write();
        for target in self.thread_targets() {
            let tid_str = target.vpid().to_string();
            cached_children.entry(tid_str).or_insert_with(|| {
                TidDirOps::new_inode(target.clone(), dir.self_ref_weak().clone())
            });
        }
    }
}

/// /proc/[pid]/task/[tid] 目录的 DirOps 实现
#[derive(Debug)]
pub struct TidDirOps {
    target: ProcPidTarget,
}

impl TidDirOps {
    pub fn new_inode(target: ProcPidTarget, parent: Weak<dyn IndexNode>) -> Arc<dyn IndexNode> {
        ProcDirBuilder::new(Self { target }, InodeMode::from_bits_truncate(0o555))
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
    )] = &[
        ("stat", |ops, parent| {
            StatFileOps::new_inode(ops.target.clone(), parent)
        }),
        ("ns", |ops, parent| {
            NsDirOps::new_inode(ops.target.clone(), parent)
        }),
        ("oom_score_adj", |ops, parent| {
            OomScoreAdjFileOps::new_inode(ops.target.clone(), parent)
        }),
    ];
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
