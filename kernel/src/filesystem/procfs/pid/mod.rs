use crate::{
    filesystem::{
        procfs::{
            template::{
                lookup_child_from_table, populate_children_from_table, DirOps, ProcDir,
                ProcDirBuilder,
            },
            Builder,
        },
        vfs::{syscall::ModeType, IndexNode},
    },
    libs::rwlock::RwLockReadGuard,
    process::{ProcessControlBlock, ProcessManager, RawPid},
};
use alloc::{
    collections::BTreeMap,
    string::{String,ToString},
    sync::{Arc, Weak},
};
use system_error::SystemError;

mod exe;
mod fd;
mod status;

use exe::ExeSymOps;
use fd::FdDirOps;
use status::StatusFileOps;

/// /proc/[pid] 目录的 DirOps 实现
#[derive(Debug)]
pub struct PidDirOps {
    // 存储 PID，用于在需要时查找进程
    pid: RawPid,
}

impl PidDirOps {
    pub fn new_inode(pid: RawPid, parent: Weak<dyn IndexNode>) -> Arc<dyn IndexNode> {
        ProcDirBuilder::new(Self { pid }, ModeType::from_bits_truncate(0o555))
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
        ("status", |ops, parent| {
            StatusFileOps::new_inode(ops.pid, parent)
        }),
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

                    fn populate_children<'a>(
                        &self,
                        dir: &'a ProcDir<Self>,
                    ) -> RwLockReadGuard<'a, BTreeMap<String, Arc<dyn IndexNode>>> {
                        dir.cached_children().write().downgrade()
                    }
                }

                ProcDirBuilder::new(EmptyDirOps, ModeType::from_bits_truncate(0o500))
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

    fn populate_children<'a>(
        &self,
        dir: &'a ProcDir<Self>,
    ) -> RwLockReadGuard<'a, BTreeMap<String, Arc<dyn IndexNode>>> {
        let mut cached_children = dir.cached_children().write();

        // 填充静态条目（包括 fd）
        populate_children_from_table(&mut cached_children, Self::STATIC_ENTRIES, |f| {
            (f)(self, dir.self_ref_weak().clone())
        });

        cached_children.downgrade()
    }
}
