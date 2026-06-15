use core::fmt;

use crate::{
    filesystem::{
        procfs::{
            template::{
                lookup_child_from_table, populate_children_from_table, DirOps, ProcDir,
                ProcDirBuilder,
            },
            Builder,
        },
        vfs::{IndexNode, InodeMode},
    },
    process::{
        namespace::pid_namespace::PidNamespace,
        pid::{Pid, PidType},
        ProcessControlBlock, RawPid,
    },
};
use alloc::sync::{Arc, Weak};
use system_error::SystemError;

mod cgroup;
mod cmdline;
mod exe;
mod fd;
mod fdinfo;
mod id_map;
mod limits;
mod maps;
mod ns;
mod oom_score_adj;
pub mod stat;
mod statm;
mod status;
mod task;

use crate::filesystem::procfs::mount::{inode::MountProcFileOps, ProcMountRenderKind};
use cgroup::CgroupFileOps;
use cmdline::CmdlineFileOps;
use exe::ExeSymOps;
use fd::FdDirOps;
use fdinfo::FdInfoDirOps;
use id_map::{IdMapFileOps, SetgroupsFileOps};
use limits::LimitsFile;
use maps::MapsFileOps;
use ns::NsDirOps;
use oom_score_adj::OomScoreAdjFileOps;
use stat::StatFileOps;
use statm::StatmFileOps;
use status::StatusFileOps;
use task::TaskDirOps;

#[derive(Clone)]
pub(super) struct ProcPidTarget {
    view_pid_ns: Arc<PidNamespace>,
    pid: Arc<Pid>,
}

impl fmt::Debug for ProcPidTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProcPidTarget")
            .field("vpid", &self.vpid())
            .field("tgid", &self.tgid())
            .finish()
    }
}

impl ProcPidTarget {
    pub fn new(view_pid_ns: Arc<PidNamespace>, pid: Arc<Pid>) -> Self {
        Self { view_pid_ns, pid }
    }

    pub fn from_tgid_in_ns(view_pid_ns: Arc<PidNamespace>, pid: RawPid) -> Option<Self> {
        let target_pid = view_pid_ns.find_pid_in_ns(pid)?;
        target_pid.pid_task(PidType::TGID)?;
        Some(Self::new(view_pid_ns, target_pid))
    }

    pub fn from_task(
        view_pid_ns: Arc<PidNamespace>,
        task: Arc<ProcessControlBlock>,
    ) -> Option<Self> {
        let pid = task.task_pid_ptr(PidType::PID)?;
        if pid.pid_nr_ns(&view_pid_ns).data() == 0 {
            return None;
        }
        Some(Self::new(view_pid_ns, pid))
    }

    pub fn view_pid_ns(&self) -> &Arc<PidNamespace> {
        &self.view_pid_ns
    }

    pub fn vpid(&self) -> RawPid {
        self.pid.pid_nr_ns(&self.view_pid_ns)
    }

    pub fn task(&self) -> Option<Arc<ProcessControlBlock>> {
        self.pid.pid_task(PidType::PID)
    }

    pub fn thread_group_leader(&self) -> Option<Arc<ProcessControlBlock>> {
        let task = self.task()?;
        let tgid = task.task_pid_ptr(PidType::TGID)?;
        tgid.pid_task(PidType::TGID)
    }

    pub fn thread_group_pid(&self) -> Option<Arc<Pid>> {
        self.thread_group_leader()?.task_pid_ptr(PidType::TGID)
    }

    pub(super) fn owner_uid_gid(&self) -> Option<(usize, usize)> {
        let pcb = self.thread_group_leader()?;
        if pcb.is_kthread() {
            return Some((0, 0));
        }
        let cred = pcb.cred();
        Some((cred.euid.data(), cred.egid.data()))
    }

    pub fn tgid(&self) -> RawPid {
        self.thread_group_pid()
            .map(|pid| pid.pid_nr_ns(&self.view_pid_ns))
            .unwrap_or(RawPid::new(0))
    }

    fn same_pid_object(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.pid, &other.pid) && Arc::ptr_eq(&self.view_pid_ns, &other.view_pid_ns)
    }
}

/// /proc/[pid] 目录的 DirOps 实现
#[derive(Debug)]
pub struct PidDirOps {
    target: ProcPidTarget,
}

impl PidDirOps {
    pub fn new_inode(target: ProcPidTarget, parent: Weak<dyn IndexNode>) -> Arc<dyn IndexNode> {
        ProcDirBuilder::new(Self { target }, InodeMode::from_bits_truncate(0o555))
            .parent(parent)
            .volatile()
            .build()
            .unwrap()
    }

    fn get_process(&self) -> Option<Arc<ProcessControlBlock>> {
        self.target.thread_group_leader()
    }

    pub(super) fn is_current_target(&self) -> bool {
        ProcPidTarget::from_tgid_in_ns(self.target.view_pid_ns().clone(), self.target.vpid())
            .map(|target| self.target.same_pid_object(&target))
            .unwrap_or(false)
    }

    #[expect(clippy::type_complexity)]
    const STATIC_ENTRIES: &'static [(
        &'static str,
        fn(&PidDirOps, Weak<dyn IndexNode>) -> Arc<dyn IndexNode>,
    )] = &[
        ("cmdline", |ops, parent| {
            CmdlineFileOps::new_inode(ops.target.clone(), parent)
        }),
        ("cgroup", |ops, parent| {
            CgroupFileOps::new_inode(ops.target.clone(), parent)
        }),
        ("maps", |ops, parent| {
            MapsFileOps::new_inode(ops.target.clone(), parent)
        }),
        ("limits", |ops, parent| {
            LimitsFile::new_inode(ops.target.clone(), parent)
        }),
        ("mountinfo", |ops, parent| {
            MountProcFileOps::new_inode(ops.target.clone(), ProcMountRenderKind::MountInfo, parent)
        }),
        ("mounts", |ops, parent| {
            MountProcFileOps::new_inode(ops.target.clone(), ProcMountRenderKind::Mounts, parent)
        }),
        ("mountstats", |ops, parent| {
            MountProcFileOps::new_inode(ops.target.clone(), ProcMountRenderKind::MountStats, parent)
        }),
        ("oom_score_adj", |ops, parent| {
            OomScoreAdjFileOps::new_inode(ops.target.clone(), parent)
        }),
        ("ns", |ops, parent| {
            NsDirOps::new_inode(ops.target.clone(), parent)
        }),
        ("stat", |ops, parent| {
            StatFileOps::new_inode(ops.target.clone(), parent)
        }),
        ("statm", |ops, parent| {
            StatmFileOps::new_inode(ops.target.clone(), parent)
        }),
        ("status", |ops, parent| {
            StatusFileOps::new_inode(ops.target.clone(), parent)
        }),
        ("uid_map", |ops, parent| {
            IdMapFileOps::new_uid_inode(ops.target.clone(), parent)
        }),
        ("gid_map", |ops, parent| {
            IdMapFileOps::new_gid_inode(ops.target.clone(), parent)
        }),
        ("setgroups", |ops, parent| {
            SetgroupsFileOps::new_inode(ops.target.clone(), parent)
        }),
        ("task", |ops, parent| {
            TaskDirOps::new_inode(ops.target.clone(), parent)
        }),
        ("exe", |ops, parent| {
            ExeSymOps::new_inode(ops.target.clone(), parent)
        }),
        ("fd", |ops, parent| {
            if ops.get_process().is_some() {
                FdDirOps::new_inode(ops.target.clone(), parent)
            } else {
                use crate::filesystem::procfs::template::ProcDirBuilder;

                #[derive(Debug)]
                struct EmptyDirOps;
                impl DirOps for EmptyDirOps {
                    fn lookup_child(
                        &self,
                        _dir: &ProcDir<Self>,
                        _name: &str,
                    ) -> Result<Arc<dyn IndexNode>, SystemError> {
                        Err(SystemError::ENOENT)
                    }

                    fn populate_children(&self, _dir: &ProcDir<Self>) {}
                }

                ProcDirBuilder::new(EmptyDirOps, InodeMode::from_bits_truncate(0o500))
                    .parent(parent)
                    .build()
                    .unwrap()
            }
        }),
        ("fdinfo", |ops, parent| {
            if ops.get_process().is_some() {
                FdInfoDirOps::new_inode(ops.target.clone(), parent)
            } else {
                use crate::filesystem::procfs::template::ProcDirBuilder;

                #[derive(Debug)]
                struct EmptyDirOps;
                impl DirOps for EmptyDirOps {
                    fn lookup_child(
                        &self,
                        _dir: &ProcDir<Self>,
                        _name: &str,
                    ) -> Result<Arc<dyn IndexNode>, SystemError> {
                        Err(SystemError::ENOENT)
                    }

                    fn populate_children(&self, _dir: &ProcDir<Self>) {}
                }

                ProcDirBuilder::new(EmptyDirOps, InodeMode::from_bits_truncate(0o500))
                    .parent(parent)
                    .build()
                    .unwrap()
            }
        }),
    ];
}

impl DirOps for PidDirOps {
    fn owner(&self) -> Option<(usize, usize)> {
        self.target.owner_uid_gid()
    }

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
