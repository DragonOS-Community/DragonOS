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
    process::{pid::PidType, ProcessControlBlock, RawPid},
};
use alloc::{
    collections::BTreeMap,
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

    /// Resolve a live TID in this proc mount's PID namespace and verify that
    /// it still belongs to the thread group represented by this task directory.
    fn current_thread_target(&self, tid: RawPid) -> Option<ProcPidTarget> {
        let leader = self.thread_group_leader()?;
        let leader_tgid = leader.task_pid_ptr(PidType::TGID)?;
        let pid = self.target.view_pid_ns().find_pid_in_ns(tid)?;
        let task = pid.pid_task(PidType::PID)?;
        let task_tgid = task.task_pid_ptr(PidType::TGID)?;
        if !Arc::ptr_eq(&leader_tgid, &task_tgid) {
            return None;
        }

        Some(ProcPidTarget::new(self.target.view_pid_ns().clone(), pid))
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

        let tid = name.parse::<RawPid>().map_err(|_| SystemError::ENOENT)?;

        let mut cached_children = dir.cached_children().write();
        let target = self.current_thread_target(tid).ok_or(SystemError::ENOENT)?;
        if let Some(child) = cached_children.get(name) {
            if let Some(tid_dir) = child.downcast_ref::<ProcDir<TidDirOps>>() {
                if tid_dir.ops().represents(&target) {
                    return Ok(child.clone());
                }
            }
        }

        if target.task().is_none() {
            return Err(SystemError::ENOENT);
        }
        let inode = TidDirOps::new_inode(target, dir.self_ref_weak().clone());
        cached_children.insert(name.to_string(), inode.clone());
        Ok(inode)
    }

    fn populate_children(&self, dir: &ProcDir<Self>) {
        if self.thread_group_leader().is_none() {
            return;
        }

        let targets = self
            .thread_targets()
            .into_iter()
            .map(|target| (target.vpid().to_string(), target))
            .collect::<BTreeMap<_, _>>();

        let mut cached_children = dir.cached_children().write();
        cached_children.retain(|name, child| {
            let Some(target) = targets.get(name) else {
                return false;
            };
            child
                .downcast_ref::<ProcDir<TidDirOps>>()
                .is_some_and(|tid_dir| tid_dir.ops().represents(target))
        });

        for (name, target) in targets {
            let needs_refresh = cached_children
                .get(&name)
                .and_then(|child| child.downcast_ref::<ProcDir<TidDirOps>>())
                .map(|tid_dir| !tid_dir.ops().represents(&target))
                .unwrap_or(true);
            if needs_refresh && target.task().is_some() {
                cached_children.insert(
                    name,
                    TidDirOps::new_inode(target, dir.self_ref_weak().clone()),
                );
            }
        }
    }

    fn validate_child(&self, child: &dyn IndexNode) -> bool {
        let Some(tid_dir) = child.downcast_ref::<ProcDir<TidDirOps>>() else {
            return true;
        };
        let target = &tid_dir.ops().target;
        let Some(task) = target.task() else {
            return false;
        };
        let Some(task_tgid) = task.task_pid_ptr(PidType::TGID) else {
            return false;
        };
        let Some(leader_tgid) = self
            .thread_group_leader()
            .and_then(|leader| leader.task_pid_ptr(PidType::TGID))
        else {
            return false;
        };

        Arc::ptr_eq(target.view_pid_ns(), self.target.view_pid_ns())
            && Arc::ptr_eq(&task_tgid, &leader_tgid)
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

    fn represents(&self, target: &ProcPidTarget) -> bool {
        self.target.same_pid_object(target)
    }

    /// 静态条目表
    #[expect(clippy::type_complexity)]
    const STATIC_ENTRIES: &'static [(
        &'static str,
        fn(&TidDirOps, Weak<dyn IndexNode>) -> Arc<dyn IndexNode>,
    )] = &[
        ("stat", |ops, parent| {
            StatFileOps::new_inode(ops.target.clone(), super::stat::StatScope::Thread, parent)
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
