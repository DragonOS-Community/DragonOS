//! /proc/[pid]/fd - 进程文件描述符目录
//!
//! 这个目录包含了进程打开的所有文件描述符的符号链接

use crate::{
    filesystem::{
        procfs::template::{Builder, DirOps, ProcDir, ProcDirBuilder, ProcSymBuilder, SymOps},
        vfs::{IndexNode, InodeMode},
    },
    process::{ProcessControlBlock, ProcessManager, RawPid},
};
use alloc::{
    format,
    string::ToString,
    sync::{Arc, Weak},
    vec::Vec,
};
use system_error::SystemError;

/// /proc/[pid]/fd 目录的 DirOps 实现
#[derive(Debug)]
pub struct FdDirOps {
    /// 存储 PID，在需要时动态查找进程
    pid: RawPid,
}

impl FdDirOps {
    pub fn new_inode(
        process_ref: Arc<ProcessControlBlock>,
        parent: Weak<dyn IndexNode>,
    ) -> Arc<dyn IndexNode> {
        let pid = process_ref.raw_pid();
        ProcDirBuilder::new(Self { pid }, InodeMode::from_bits_truncate(0o500)) // dr-x------
            .parent(parent)
            .volatile() // fd 是易失的，因为它们与特定进程关联
            .build()
            .unwrap()
    }

    /// 获取进程引用
    fn get_process(&self) -> Option<Arc<ProcessControlBlock>> {
        ProcessManager::find(self.pid)
    }
}

impl DirOps for FdDirOps {
    fn lookup_child(
        &self,
        dir: &ProcDir<Self>,
        name: &str,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        // 解析文件描述符编号
        let fd = name.parse::<i32>().map_err(|_| SystemError::ENOENT)?;

        // 获取进程引用
        let process = self.get_process().ok_or(SystemError::ESRCH)?;

        // 检查文件描述符是否存在，并立即释放fd_table锁
        {
            let fd_table = process.fd_table();
            let fd_table_guard = fd_table.read();

            if fd_table_guard.get_file_by_fd(fd).is_none() {
                return Err(SystemError::ENOENT);
            }
        } // fd_table_guard在这里被释放

        // 创建或获取缓存的符号链接
        let mut cached_children = dir.cached_children().write();

        if let Some(child) = cached_children.get(name) {
            return Ok(child.clone());
        }

        // 创建新的符号链接（传递 PID 和 fd）
        let inode = FdSymOps::new_inode(self.pid, fd, dir.self_ref_weak().clone());
        cached_children.insert(name.to_string(), inode.clone());

        Ok(inode)
    }

    fn populate_children(&self, dir: &ProcDir<Self>) {
        let mut cached_children = dir.cached_children().write();

        // 清空现有缓存
        cached_children.clear();

        // 获取进程的所有文件描述符
        if let Some(process) = self.get_process() {
            // 先收集所有的fd，避免在持有锁时做复杂操作
            let fds: Vec<i32> = {
                let fd_table = process.fd_table();
                let fd_table_guard = fd_table.read();
                fd_table_guard.iter().map(|(fd, _)| fd).collect()
            };

            // 现在fd_table锁已经释放，我们可以安全地创建inodes
            for fd in fds {
                let fd_str = fd.to_string();

                // 创建或获取缓存的符号链接（传递 PID 和 fd）
                cached_children.entry(fd_str.clone()).or_insert_with(|| {
                    FdSymOps::new_inode(self.pid, fd, dir.self_ref_weak().clone())
                });
            }
        }
        // 写锁在这里自动释放
    }
}

/// /proc/[pid]/fd/[fd] 符号链接的 SymOps 实现
#[derive(Debug)]
pub struct FdSymOps {
    /// 存储 PID，在需要时动态查找进程
    pid: RawPid,
    fd: i32,
}

impl FdSymOps {
    pub fn new_inode(pid: RawPid, fd: i32, parent: Weak<dyn IndexNode>) -> Arc<dyn IndexNode> {
        ProcSymBuilder::new(Self { pid, fd }, InodeMode::from_bits_truncate(0o700)) // lrwx------
            .parent(parent)
            .volatile()
            .build()
            .unwrap()
    }
}

impl SymOps for FdSymOps {
    fn read_link(&self, buf: &mut [u8]) -> Result<usize, SystemError> {
        // 动态查找进程
        let process = ProcessManager::find(self.pid).ok_or(SystemError::ESRCH)?;

        // 先获取文件对象的 clone，然后立即释放 fd_table 锁
        // 避免在持有锁时调用可能获取其他锁的方法（如 absolute_path）
        let file = {
            let fd_table = process.fd_table();
            let fd_table_guard = fd_table.read();

            fd_table_guard
                .get_file_by_fd(self.fd)
                .ok_or(SystemError::EBADF)?
        }; // fd_table 锁在这里被释放

        // 现在安全地获取文件的路径
        let path = if let Ok(path) = file.inode().absolute_path() {
            path
        } else {
            // 匿名文件或无法获取路径
            let inode_id = file.inode().metadata()?.inode_id;
            format!("anon_inode:[{}]", inode_id)
        };

        // 复制路径到缓冲区
        let path_bytes = path.as_bytes();
        let copy_len = path_bytes.len().min(buf.len());
        buf[..copy_len].copy_from_slice(&path_bytes[..copy_len]);

        Ok(copy_len)
    }
}
