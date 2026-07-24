//! /proc/[pid]/fdinfo - 进程文件描述符信息目录
//!
//! 这个目录包含了进程打开的所有文件描述符的详细信息

use crate::filesystem::{
    procfs::{
        pid::ProcPidTarget,
        template::{Builder, DirOps, FileOps, ProcDir, ProcDirBuilder, ProcFile, ProcFileBuilder},
    },
    vfs::{FilePrivateData, IndexNode, InodeMode},
};
use crate::libs::mutex::MutexGuard;
use alloc::{
    string::ToString,
    sync::{Arc, Weak},
    vec::Vec,
};
use system_error::SystemError;

/// /proc/[pid]/fdinfo 目录的 DirOps 实现
#[derive(Debug)]
pub struct FdInfoDirOps {
    target: ProcPidTarget,
}

impl FdInfoDirOps {
    pub fn new_inode(target: ProcPidTarget, parent: Weak<dyn IndexNode>) -> Arc<dyn IndexNode> {
        ProcDirBuilder::new(Self { target }, InodeMode::from_bits_truncate(0o555))
            .parent(parent)
            .volatile()
            .build()
            .unwrap()
    }

    fn get_process(&self) -> Option<Arc<crate::process::ProcessControlBlock>> {
        self.target.thread_group_leader()
    }
}

impl DirOps for FdInfoDirOps {
    fn lookup_child(
        &self,
        dir: &ProcDir<Self>,
        name: &str,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        // 解析文件描述符编号
        let fd = name.parse::<i32>().map_err(|_| SystemError::ENOENT)?;

        // 获取进程引用
        let process = self.get_process().ok_or(SystemError::ENOENT)?;

        // 检查文件描述符是否存在
        {
            // The process may have reached exit_files() after the proc target
            // was resolved. A disappearing fd table is normal procfs churn,
            // not an invariant which may be unwrapped.
            let fd_table = process
                .basic()
                .try_fd_table()
                .clone()
                .ok_or(SystemError::ENOENT)?;
            let fd_table_guard = fd_table.read();

            if fd_table_guard.get_file_by_fd(fd).is_none() {
                return Err(SystemError::ENOENT);
            }
        }

        // 创建或获取缓存的文件
        let mut cached_children = dir.cached_children().write();

        if let Some(child) = cached_children.get(name) {
            return Ok(child.clone());
        }

        // 创建新的 fdinfo 文件
        let inode = FdInfoFileOps::new_inode(self.target.clone(), fd, dir.self_ref_weak().clone());
        cached_children.insert(name.to_string(), inode.clone());

        Ok(inode)
    }

    fn populate_children(&self, dir: &ProcDir<Self>) {
        let mut cached_children = dir.cached_children().write();

        // 清空现有缓存
        cached_children.clear();

        // 获取进程的所有文件描述符
        if let Some(process) = self.get_process() {
            let Some(fd_table) = process.basic().try_fd_table().clone() else {
                return;
            };
            let fds: Vec<i32> = {
                let fd_table_guard = fd_table.read();
                fd_table_guard.iter().map(|(fd, _)| fd).collect()
            };

            for fd in fds {
                let fd_str = fd.to_string();
                cached_children.entry(fd_str.clone()).or_insert_with(|| {
                    FdInfoFileOps::new_inode(self.target.clone(), fd, dir.self_ref_weak().clone())
                });
            }
        }
    }

    fn validate_child(&self, child: &dyn IndexNode) -> bool {
        child
            .downcast_ref::<ProcFile<FdInfoFileOps>>()
            .is_some_and(|file| file.ops().is_current())
    }
}

/// /proc/[pid]/fdinfo/[fd] 文件的 FileOps 实现
#[derive(Debug)]
pub struct FdInfoFileOps {
    target: ProcPidTarget,
    fd: i32,
}

impl FdInfoFileOps {
    pub fn new_inode(
        target: ProcPidTarget,
        fd: i32,
        parent: Weak<dyn IndexNode>,
    ) -> Arc<dyn IndexNode> {
        ProcFileBuilder::new(Self { target, fd }, InodeMode::S_IRUGO)
            .parent(parent)
            .build()
            .unwrap()
    }

    fn is_current(&self) -> bool {
        let Some(process) = self.target.thread_group_leader() else {
            return false;
        };
        let Some(fd_table) = process.basic().try_fd_table().clone() else {
            return false;
        };
        let fd_table_guard = fd_table.read();
        fd_table_guard.get_file_by_fd(self.fd).is_some()
    }
}

impl FileOps for FdInfoFileOps {
    fn open(&self, _data: &mut MutexGuard<FilePrivateData>) -> Result<(), SystemError> {
        if self.is_current() {
            Ok(())
        } else {
            Err(SystemError::ENOENT)
        }
    }

    fn read_at(
        &self,
        _offset: usize,
        _len: usize,
        _buf: &mut [u8],
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        if self.is_current() {
            Ok(0)
        } else {
            Err(SystemError::ENOENT)
        }
    }
}
