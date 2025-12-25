//! /proc/[pid]/fdinfo - 进程文件描述符信息目录
//!
//! 这个目录包含了进程打开的所有文件描述符的详细信息

use crate::{
    filesystem::{
        procfs::template::{Builder, DirOps, FileOps, ProcDir, ProcDirBuilder, ProcFileBuilder},
        vfs::{FilePrivateData, IndexNode, InodeMode},
    },
    libs::spinlock::SpinLockGuard,
    process::{ProcessControlBlock, ProcessManager, RawPid},
};
use alloc::{
    string::ToString,
    sync::{Arc, Weak},
    vec::Vec,
};
use system_error::SystemError;

/// /proc/[pid]/fdinfo 目录的 DirOps 实现
#[derive(Debug)]
pub struct FdInfoDirOps {
    /// 存储 PID，在需要时动态查找进程
    pid: RawPid,
}

impl FdInfoDirOps {
    pub fn new_inode(
        process_ref: Arc<ProcessControlBlock>,
        parent: Weak<dyn IndexNode>,
    ) -> Arc<dyn IndexNode> {
        let pid = process_ref.raw_pid();
        ProcDirBuilder::new(Self { pid }, InodeMode::from_bits_truncate(0o555))
            .parent(parent)
            .volatile()
            .build()
            .unwrap()
    }

    /// 获取进程引用
    fn get_process(&self) -> Option<Arc<ProcessControlBlock>> {
        ProcessManager::find(self.pid)
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
        let process = self.get_process().ok_or(SystemError::ESRCH)?;

        // 检查文件描述符是否存在
        {
            let fd_table = process.fd_table();
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
        let inode = FdInfoFileOps::new_inode(self.pid, fd, dir.self_ref_weak().clone());
        cached_children.insert(name.to_string(), inode.clone());

        Ok(inode)
    }

    fn populate_children(&self, dir: &ProcDir<Self>) {
        let mut cached_children = dir.cached_children().write();

        // 清空现有缓存
        cached_children.clear();

        // 获取进程的所有文件描述符
        if let Some(process) = self.get_process() {
            let fds: Vec<i32> = {
                let fd_table = process.fd_table();
                let fd_table_guard = fd_table.read();
                fd_table_guard.iter().map(|(fd, _)| fd).collect()
            };

            for fd in fds {
                let fd_str = fd.to_string();
                cached_children.entry(fd_str.clone()).or_insert_with(|| {
                    FdInfoFileOps::new_inode(self.pid, fd, dir.self_ref_weak().clone())
                });
            }
        }
    }
}

/// /proc/[pid]/fdinfo/[fd] 文件的 FileOps 实现
#[derive(Debug)]
pub struct FdInfoFileOps {
    /// 存储 PID，在需要时动态查找进程
    pid: RawPid,
    fd: i32,
}

impl FdInfoFileOps {
    pub fn new_inode(pid: RawPid, fd: i32, parent: Weak<dyn IndexNode>) -> Arc<dyn IndexNode> {
        ProcFileBuilder::new(Self { pid, fd }, InodeMode::S_IRUGO)
            .parent(parent)
            .build()
            .unwrap()
    }
}

impl FileOps for FdInfoFileOps {
    fn read_at(
        &self,
        _offset: usize,
        _len: usize,
        _buf: &mut [u8],
        _data: SpinLockGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        // // 动态查找进程
        // let process = ProcessManager::find(self.pid).ok_or(SystemError::ESRCH)?;

        // // 获取文件信息
        // let file = {
        //     let fd_table = process.fd_table();
        //     let fd_table_guard = fd_table.read();
        //     fd_table_guard
        //         .get_file_by_fd(self.fd)
        //         .ok_or(SystemError::EBADF)?
        // };

        // // 生成 fdinfo 内容
        // let mut content = Vec::new();

        // // pos: 当前文件偏移量
        // let pos = file.offset;
        // content.extend_from_slice(format!("pos:\t{}\n", pos).as_bytes());

        // // flags: 文件打开标志（八进制）
        // let flags = file.mode().bits();
        // content.extend_from_slice(format!("flags:\t{:#o}\n", flags).as_bytes());

        // // mnt_id: 挂载点 ID（简化实现，返回 0）
        // content.extend_from_slice(b"mnt_id:\t0\n");

        // proc_read(offset, len, buf, &content)
        Ok(0)
    }
}
