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
    process::{ProcessControlBlock, ProcessManager, RawPid},
};
use alloc::sync::{Arc, Weak};
use system_error::SystemError;

mod cmdline;
mod exe;
mod fd;
mod fdinfo;
mod maps;
mod mountinfo;
mod mounts;
mod ns;
pub mod stat;
mod statm;
mod status;
mod task;

use cmdline::CmdlineFileOps;
use exe::ExeSymOps;
use fd::FdDirOps;
use fdinfo::FdInfoDirOps;
use maps::MapsFileOps;
use mountinfo::MountInfoFileOps;
use mounts::PidMountsFileOps;
use ns::NsDirOps;
use stat::StatFileOps;
use statm::StatmFileOps;
use status::StatusFileOps;
use task::TaskDirOps;

/// /proc/[pid] 目录的 DirOps 实现
#[derive(Debug)]
pub struct PidDirOps {
    // 存储 PID，用于在需要时查找进程
    pid: RawPid,
}

impl PidDirOps {
    pub fn new_inode(pid: RawPid, parent: Weak<dyn IndexNode>) -> Arc<dyn IndexNode> {
        ProcDirBuilder::new(Self { pid }, InodeMode::from_bits_truncate(0o555))
            .parent(parent)
            .volatile() // PID 目录是易失的，因为它们与特定进程关联
            .build()
            .unwrap()
    }

    /// 获取进程引用
    fn get_process(&self) -> Option<Arc<ProcessControlBlock>> {
        ProcessManager::find(self.pid)
    }

    /// 静态条目表
    /// 包含 /proc/[pid] 目录下的所有静态文件和目录
    #[expect(clippy::type_complexity)]
    const STATIC_ENTRIES: &'static [(
        &'static str,
        fn(&PidDirOps, Weak<dyn IndexNode>) -> Arc<dyn IndexNode>,
    )] = &[
        ("cmdline", |ops, parent| {
            CmdlineFileOps::new_inode(ops.pid, parent)
        }),
        ("maps", |ops, parent| {
            MapsFileOps::new_inode(ops.pid, parent)
        }),
        ("mountinfo", |ops, parent| {
            MountInfoFileOps::new_inode(ops.pid, parent)
        }),
        ("mounts", |ops, parent| {
            PidMountsFileOps::new_inode(ops.pid, parent)
        }),
        ("ns", |ops, parent| NsDirOps::new_inode(ops.pid, parent)),
        ("stat", |ops, parent| {
            StatFileOps::new_inode(ops.pid, parent)
        }),
        ("statm", |ops, parent| {
            StatmFileOps::new_inode(ops.pid, parent)
        }),
        ("status", |ops, parent| {
            StatusFileOps::new_inode(ops.pid, parent)
        }),
        ("task", |ops, parent| TaskDirOps::new_inode(ops.pid, parent)),
        ("exe", |ops, parent| ExeSymOps::new_inode(ops.pid, parent)),
        ("fd", |ops, parent| {
            // fd 目录仍然需要进程引用来列出文件描述符
            if let Some(process) = ops.get_process() {
                FdDirOps::new_inode(process, parent)
            } else {
                // 进程已退出，创建空目录
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

                    fn populate_children(&self, _dir: &ProcDir<Self>) {
                        // 空目录，无需填充
                    }
                }

                ProcDirBuilder::new(EmptyDirOps, InodeMode::from_bits_truncate(0o500))
                    .parent(parent)
                    .build()
                    .unwrap()
            }
        }),
        ("fdinfo", |ops, parent| {
            // fdinfo 目录也需要进程引用来列出文件描述符
            if let Some(process) = ops.get_process() {
                FdInfoDirOps::new_inode(process, parent)
            } else {
                // 进程已退出，创建空目录
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

                    fn populate_children(&self, _dir: &ProcDir<Self>) {
                        // 空目录，无需填充
                    }
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
    fn lookup_child(
        &self,
        dir: &ProcDir<Self>,
        name: &str,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        let mut cached_children = dir.cached_children().write();

        // 处理静态条目（包括 fd）
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

        // 填充静态条目（包括 fd）
        populate_children_from_table(&mut cached_children, Self::STATIC_ENTRIES, |f| {
            (f)(self, dir.self_ref_weak().clone())
        });
        // 写锁在这里自动释放
    }
}
